use std::{
    collections::{BTreeMap, HashMap, HashSet},
    mem,
    sync::{Arc, RwLock},
    time::SystemTime,
};

use ahash::RandomState;
use anyhow::{bail, Context as AnyhowContext, Result};
use arrow::{
    compute::{kernels, partition, sort_to_indices, take},
    row::{RowConverter, SortField},
};
use arrow_array::{
    types::{GenericBinaryType, Int64Type, TimestampNanosecondType, UInt64Type},
    Array, ArrayRef, GenericByteArray, NullArray, PrimitiveArray, RecordBatch,
};
use arrow_schema::{DataType, Field, FieldRef, Schema, SchemaRef, TimeUnit};
use arroyo_df::schemas::{add_timestamp_field_arrow, window_arrow_struct};
use arroyo_rpc::{
    grpc::{
        api, api::window::Window, TableConfig, TableDeleteBehavior, TableDescriptor, TableType,
        TableWriteBehavior,
    },
    ArroyoSchema,
};
use arroyo_state::{
    parquet::{ParquetStats, RecordBatchBuilder},
    tables::expiring_time_key_map,
    timestamp_table_config, DataOperation,
};
use arroyo_types::{
    from_nanos, to_nanos, ArrowMessage, CheckpointBarrier, Record, RecordBatchData, SignalMessage,
    Watermark,
};
use bincode::config;
use datafusion::{
    execution::context::SessionContext,
    physical_plan::{stream::RecordBatchStreamAdapter, DisplayAs, ExecutionPlan},
};
use datafusion_common::{
    hash_utils::create_hashes, DFField, DFSchema, DataFusionError, ScalarValue,
};

use crate::engine::ArrowContext;
use crate::old::Context;
use crate::operator::{ArrowOperator, ArrowOperatorConstructor, OperatorNode};
use arroyo_df::physical::{ArroyoMemExec, ArroyoPhysicalExtensionCodec, DecodingContext};
use datafusion_execution::{
    runtime_env::{RuntimeConfig, RuntimeEnv},
    FunctionRegistry, SendableRecordBatchStream,
};
use datafusion_expr::{AggregateUDF, ScalarUDF, WindowUDF};
use datafusion_physical_expr::PhysicalExpr;
use datafusion_proto::{
    physical_plan::{from_proto::parse_physical_expr, AsExecutionPlan},
    protobuf::{
        physical_plan_node::PhysicalPlanType, AggregateMode, PhysicalExprNode, PhysicalPlanNode,
    },
};
use prost::Message;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use tracing::info;

pub struct TumblingAggregatingWindowFunc {
    width: Duration,
    binning_function: Arc<dyn PhysicalExpr>,
    partial_aggregation_plan: Arc<dyn ExecutionPlan>,
    partial_schema: ArroyoSchema,
    finish_execution_plan: Arc<dyn ExecutionPlan>,
    // the partial aggregation plan shares a reference to it,
    // which is only used on the exec()
    receiver: Arc<RwLock<Option<UnboundedReceiver<RecordBatch>>>>,
    final_batches_passer: Arc<RwLock<Vec<RecordBatch>>>,
    senders: BTreeMap<usize, UnboundedSender<RecordBatch>>,
    execs: BTreeMap<usize, BinComputingHolder>,
    window_field: FieldRef,
    window_index: usize,
}

impl TumblingAggregatingWindowFunc {
    fn time_to_bin(&self, time: SystemTime) -> usize {
        (to_nanos(time) / self.width.as_nanos()) as usize
    }
}

#[derive(Default)]
struct BinComputingHolder {
    active_exec: Option<SendableRecordBatchStream>,
    finished_batches: Vec<RecordBatch>,
}

pub struct Registry {}

impl FunctionRegistry for Registry {
    fn udfs(&self) -> HashSet<String> {
        HashSet::new()
    }

    fn udf(&self, _name: &str) -> datafusion_common::Result<Arc<ScalarUDF>> {
        todo!()
    }

    fn udaf(&self, _name: &str) -> datafusion_common::Result<Arc<AggregateUDF>> {
        todo!()
    }

    fn udwf(&self, _name: &str) -> datafusion_common::Result<Arc<WindowUDF>> {
        todo!()
    }
}

impl ArrowOperatorConstructor<api::WindowAggregateOperator> for TumblingAggregatingWindowFunc {
    fn from_config(proto_config: api::WindowAggregateOperator) -> Result<OperatorNode> {
        let registry = Registry {};

        let binning_function =
            PhysicalExprNode::decode(&mut proto_config.binning_function.as_slice()).unwrap();
        let binning_schema: Schema =
            serde_json::from_slice(proto_config.binning_schema.as_slice())?;

        let binning_function =
            parse_physical_expr(&binning_function, &Registry {}, &binning_schema)?;

        let physical_plan =
            PhysicalPlanNode::decode(&mut proto_config.physical_plan.as_slice()).unwrap();

        let Window::TumblingWindow(window) = proto_config.window.unwrap().window.unwrap() else {
            bail!("expected tumbling window")
        };
        let window_field = Arc::new(Field::new(
            proto_config.window_field_name,
            window_arrow_struct(),
            true,
        ));

        let key_indices: Vec<_> = proto_config
            .key_fields
            .into_iter()
            .map(|x| x as usize)
            .collect();
        let input_schema: Schema = serde_json::from_slice(proto_config.input_schema.as_slice())
            .context(format!(
                "failed to deserialize schema of length {}",
                proto_config.input_schema.len()
            ))?;
        let timestamp_index = input_schema.index_of("_timestamp")?;
        let value_indices: Vec<_> = (0..input_schema.fields().len())
            .filter(|index| !key_indices.contains(index) && timestamp_index != *index)
            .collect();

        let receiver = Arc::new(RwLock::new(None));
        let final_batches_passer = Arc::new(RwLock::new(Vec::new()));

        let (partial_aggregation_plan, finish_execution_plan) = match physical_plan
            .physical_plan_type
            .as_ref()
            .unwrap()
        {
            PhysicalPlanType::ParquetScan(_) => todo!(),
            PhysicalPlanType::CsvScan(_) => todo!(),
            PhysicalPlanType::Empty(_) => todo!(),
            PhysicalPlanType::Projection(_) => todo!(),
            PhysicalPlanType::GlobalLimit(_) => todo!(),
            PhysicalPlanType::LocalLimit(_) => todo!(),
            PhysicalPlanType::Aggregate(aggregate) => {
                let AggregateMode::Final = aggregate.mode() else {
                    bail!("expect AggregateMode to be Final so we can decompose it for checkpointing.")
                };
                let mut top_level_copy = aggregate.as_ref().clone();

                let partial_aggregation_plan = aggregate.input.as_ref().unwrap().as_ref().clone();

                let codec = ArroyoPhysicalExtensionCodec {
                    context: DecodingContext::UnboundedBatchStream(receiver.clone()),
                };

                let partial_aggregation_plan = partial_aggregation_plan.try_into_physical_plan(
                    &Registry {},
                    &RuntimeEnv::new(RuntimeConfig::new()).unwrap(),
                    &codec,
                )?;
                let partial_schema = partial_aggregation_plan.schema();
                let table_provider = ArroyoMemExec {
                    table_name: "partial".into(),
                    schema: partial_schema,
                };
                let wrapped = Arc::new(table_provider);

                top_level_copy.input = Some(Box::new(PhysicalPlanNode::try_from_physical_plan(
                    wrapped,
                    &ArroyoPhysicalExtensionCodec::default(),
                )?));

                let finish_plan = PhysicalPlanNode {
                    physical_plan_type: Some(PhysicalPlanType::Aggregate(Box::new(top_level_copy))),
                };

                let final_codec = ArroyoPhysicalExtensionCodec {
                    context: DecodingContext::LockedBatchVec(final_batches_passer.clone()),
                };

                let finish_execution_plan = finish_plan.try_into_physical_plan(
                    &Registry {},
                    &RuntimeEnv::new(RuntimeConfig::new()).unwrap(),
                    &final_codec,
                )?;

                (partial_aggregation_plan, finish_execution_plan)
            }
            PhysicalPlanType::HashJoin(_) => todo!(),
            PhysicalPlanType::Sort(_) => todo!(),
            PhysicalPlanType::CoalesceBatches(_) => todo!(),
            PhysicalPlanType::Filter(_) => todo!(),
            PhysicalPlanType::Merge(_) => todo!(),
            PhysicalPlanType::Repartition(_) => todo!(),
            PhysicalPlanType::Window(_) => todo!(),
            PhysicalPlanType::CrossJoin(_) => todo!(),
            PhysicalPlanType::AvroScan(_) => todo!(),
            PhysicalPlanType::Extension(_) => todo!(),
            PhysicalPlanType::Union(_) => todo!(),
            PhysicalPlanType::Explain(_) => todo!(),
            PhysicalPlanType::SortPreservingMerge(_) => todo!(),
            PhysicalPlanType::NestedLoopJoin(_) => todo!(),
            PhysicalPlanType::Analyze(_) => todo!(),
            PhysicalPlanType::JsonSink(_) => todo!(),
            PhysicalPlanType::SymmetricHashJoin(_) => todo!(),
            PhysicalPlanType::Interleave(_) => todo!(),
            PhysicalPlanType::PlaceholderRow(_) => todo!(),
        };

        let schema_ref = partial_aggregation_plan.schema();
        let partial_schema = add_timestamp_field_arrow(schema_ref);
        let timestamp_index = partial_schema.fields().len() - 1;
        let partial_schema = ArroyoSchema {
            schema: partial_schema,
            timestamp_index,
            key_indices,
        };

        Ok(OperatorNode::from_operator(Box::new(Self {
            width: Duration::from_micros(window.size_micros),
            binning_function,
            partial_aggregation_plan,
            partial_schema,
            finish_execution_plan,
            receiver,
            final_batches_passer,
            senders: BTreeMap::new(),
            execs: BTreeMap::new(),
            window_field,
            window_index: proto_config.window_index as usize,
        })))
    }
}

#[derive(Debug)]
enum TumblingWindowState {
    // We haven't received any data.
    NoData,
    // We've received data, but don't have any data in the memory_view.
    BufferedData { earliest_bin_time: SystemTime },
}
struct BinAggregator {
    sender: UnboundedSender<RecordBatch>,
    aggregate_exec: Arc<dyn ExecutionPlan>,
}

#[async_trait::async_trait]

impl ArrowOperator for TumblingAggregatingWindowFunc {
    fn name(&self) -> String {
        "tumbling_window".to_string()
    }

    async fn on_start(&mut self, ctx: &mut ArrowContext) {
        let watermark = ctx.last_present_watermark();
        let table = ctx
            .table_manager
            .get_expiring_time_key_table("t", watermark)
            .await
            .expect("should be able to load table");
        for (timestamp, batch) in table.all_batches_for_watermark(watermark) {
            let bin = self.time_to_bin(*timestamp);
            let holder = self.execs.entry(bin).or_default();
            batch
                .iter()
                .for_each(|batch| holder.finished_batches.push(batch.clone()));
        }
    }

    async fn process_batch(&mut self, batch: RecordBatch, ctx: &mut ArrowContext) {
        /*if batch.num_rows() > 0 {
            let (record_batch, parquet_stats) = self.converter_tools.get_state_record_batch(batch);
            ctx.state
                .insert_record_batch('s', record_batch, parquet_stats)
                .await;
        }*/
        let timestamp_column = batch
            .column_by_name("_timestamp")
            .unwrap()
            .as_any()
            .downcast_ref::<PrimitiveArray<TimestampNanosecondType>>()
            .unwrap();
        let timestamp_nanos_column: PrimitiveArray<Int64Type> = timestamp_column.reinterpret_cast();
        let timestamp_nanos_field =
            DFField::new_unqualified("timestamp_nanos", DataType::Int64, false);
        let df_schema = DFSchema::new_with_metadata(vec![timestamp_nanos_field], HashMap::new())
            .expect("can't make timestamp nanos schema");
        let timestamp_batch = RecordBatch::try_new(
            Arc::new((&df_schema).into()),
            vec![Arc::new(timestamp_nanos_column)],
        )
        .unwrap();
        let bin = self
            .binning_function
            .evaluate(&timestamp_batch)
            .unwrap()
            .into_array(batch.num_rows())
            .unwrap();
        let indices = sort_to_indices(bin.as_ref(), None, None).unwrap();
        let columns = batch
            .columns()
            .iter()
            .map(|c| take(c, &indices, None).unwrap())
            .collect();
        let sorted = RecordBatch::try_new(batch.schema(), columns).unwrap();
        let sorted_bins = take(&*bin, &indices, None).unwrap();

        let partition = partition(vec![sorted_bins.clone()].as_slice()).unwrap();
        let typed_bin = sorted_bins
            .as_any()
            .downcast_ref::<PrimitiveArray<Int64Type>>()
            .unwrap();

        for range in partition.ranges() {
            let bin = typed_bin.value(range.start) as usize;
            let bin_batch = sorted.slice(range.start, range.end - range.start);
            let bin_exec = self.execs.entry(bin).or_default();
            if bin_exec.active_exec.is_none() {
                let (unbounded_sender, unbounded_receiver) = unbounded_channel();
                self.senders.insert(bin, unbounded_sender);
                {
                    let mut internal_receiver = self.receiver.write().unwrap();
                    *internal_receiver = Some(unbounded_receiver);
                }
                bin_exec.active_exec = Some(
                    self.partial_aggregation_plan
                        .execute(0, SessionContext::new().task_ctx())
                        .unwrap(),
                );
            }
            let sender = self.senders.get(&bin).unwrap();
            sender.send(bin_batch).unwrap();
        }
    }

    async fn handle_watermark(&mut self, watermark: Watermark, ctx: &mut ArrowContext) {
        if let Watermark::EventTime(watermark) = &watermark {
            let bin = (to_nanos(*watermark) / self.width.as_nanos()) as usize;
            while !self.execs.is_empty() {
                let should_pop = {
                    let Some((first_bin, _exec)) = self.execs.first_key_value() else {
                        unreachable!("isn't empty")
                    };
                    *first_bin < bin
                };
                if should_pop {
                    let Some((popped_bin, mut exec)) = self.execs.pop_first() else {
                        unreachable!("should have an entry")
                    };
                    if let Some(mut active_exec) = exec.active_exec.take() {
                        self.senders
                            .remove(&popped_bin)
                            .expect("should have sender for bin");
                        while let Some(batch) = active_exec.next().await {
                            let batch = batch.expect("should be able to compute batch");
                            exec.finished_batches.push(batch);
                        }
                    }
                    {
                        let mut batches = self.final_batches_passer.write().unwrap();
                        let finished_batches = mem::take(&mut exec.finished_batches);
                        *batches = finished_batches;
                    }
                    let mut final_exec = self
                        .finish_execution_plan
                        .execute(0, SessionContext::new().task_ctx())
                        .unwrap();
                    while let Some(batch) = final_exec.next().await {
                        let batch = batch.expect("should be able to compute batch");
                        let bin_start = ((popped_bin) * (self.width.as_nanos() as usize)) as i64;
                        let bin_end = bin_start + (self.width.as_nanos() as i64);
                        let timestamp = bin_end - 1;
                        let timestamp_array =
                            ScalarValue::TimestampNanosecond(Some(timestamp), None)
                                .to_array_of_size(batch.num_rows())
                                .unwrap();
                        let mut fields = batch.schema().fields().as_ref().to_vec();
                        fields.push(Arc::new(Field::new(
                            "_timestamp",
                            DataType::Timestamp(TimeUnit::Nanosecond, None),
                            false,
                        )));

                        fields.insert(self.window_index, self.window_field.clone());

                        let mut columns = batch.columns().to_vec();
                        columns.push(timestamp_array);
                        let DataType::Struct(struct_fields) = self.window_field.data_type() else {
                            unreachable!("should have struct for window field type")
                        };
                        let window_scalar = ScalarValue::Struct(
                            Some(vec![
                                ScalarValue::TimestampNanosecond(Some(bin_start), None),
                                ScalarValue::TimestampNanosecond(Some(bin_end), None),
                            ]),
                            struct_fields.clone(),
                        );
                        columns.insert(
                            self.window_index,
                            window_scalar.to_array_of_size(batch.num_rows()).unwrap(),
                        );

                        let batch_with_timestamp = RecordBatch::try_new(
                            Arc::new(Schema::new_with_metadata(fields, HashMap::new())),
                            columns,
                        )
                        .unwrap();
                        ctx.collect(batch_with_timestamp).await;
                    }
                } else {
                    break;
                }
            }
        }
        // by default, just pass watermarks on down
        ctx.broadcast(ArrowMessage::Signal(SignalMessage::Watermark(watermark)))
            .await;
    }

    async fn handle_checkpoint(&mut self, b: CheckpointBarrier, ctx: &mut ArrowContext) {
        let keys: Vec<_> = self.senders.keys().cloned().collect();
        self.senders.clear();
        let watermark = ctx
            .watermark()
            .map(|watermark: Watermark| match watermark {
                Watermark::EventTime(watermark) => Some(watermark),
                Watermark::Idle => None,
            })
            .flatten();
        let table = ctx
            .table_manager
            .get_expiring_time_key_table("t", watermark)
            .await
            .expect("should get table");

        for key in keys {
            let exec = self.execs.get_mut(&key).unwrap();
            let bucket_nanos = key as i64 * (self.width.as_nanos() as i64);
            let mut active_exec = exec.active_exec.take().expect("this should be active");
            while let Some(batch) = active_exec.next().await {
                let batch = batch.expect("should be able to compute batch");
                let bin_start = ScalarValue::TimestampNanosecond(Some(bucket_nanos), None);
                let timestamp_array = bin_start.to_array_of_size(batch.num_rows()).unwrap();
                let mut columns = batch.columns().to_vec();
                columns.push(timestamp_array);
                let state_batch =
                    RecordBatch::try_new(self.partial_schema.schema.clone(), columns).unwrap();
                table.insert(from_nanos(bucket_nanos as u128), state_batch);
                exec.finished_batches.push(batch);
            }
        }
        table.flush(watermark).await.unwrap();
    }

    fn tables(&self) -> HashMap<String, TableConfig> {
        vec![(
            "t".to_string(),
            timestamp_table_config(
                "t",
                "tumbling_intermediate",
                self.width,
                self.partial_schema.clone(),
            ),
        )]
        .into_iter()
        .collect()
    }
}
