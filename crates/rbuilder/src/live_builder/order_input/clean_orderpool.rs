use super::OrderInputConfig;
use crate::{
    live_builder::order_input::orderpool::OrderPool,
    provider::StateProviderFactory,
    telemetry::{set_current_block, set_ordepool_count},
};
use ethers::{
    middleware::Middleware,
    providers::{Ipc, Provider},
};
use futures::StreamExt;
use std::{
    pin::pin,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

/// Performs maintenance operations on every new header by calling OrderPool::head_updated.
/// Also calls some functions to generate metrics.
pub async fn spawn_clean_orderpool_job<SProvider: StateProviderFactory + Clone + 'static>(
    config: OrderInputConfig,
    provider_factory: SProvider,
    orderpool: Arc<Mutex<OrderPool>>,
    global_cancellation: CancellationToken,
) -> eyre::Result<JoinHandle<()>> {
    let ipc = Ipc::connect(config.ipc_path).await?;
    let provider = Provider::new(ipc);
    {
        // quickly check that we can subscribe, before moving provider into the task
        let sub = provider.subscribe_blocks().await?;
        sub.unsubscribe().await.unwrap_or_default();
    }

    let handle = tokio::spawn(async move {
        info!("Clean orderpool job: started");

        let new_block_stream = match provider.subscribe_blocks().await {
            Ok(stream) => stream.take_until(global_cancellation.cancelled()),
            Err(err) => {
                error!("Failed to subscribe to a new block stream: {:?}", err);
                global_cancellation.cancel();
                return;
            }
        };
        let mut new_block_stream = pin!(new_block_stream);

        while let Some(block) = new_block_stream.next().await {
            let block_number = block.number.unwrap_or_default().as_u64();
            set_current_block(block_number);
            let state = match provider_factory.latest() {
                Ok(state) => state,
                Err(err) => {
                    error!("Failed to get latest state: {}", err);
                    // @Metric error count
                    continue;
                }
            };

            let mut orderpool = orderpool.lock().unwrap();
            let start = Instant::now();

            orderpool.head_updated(block_number, &state);

            let update_time = start.elapsed();
            let (tx_count, bundle_count) = orderpool.content_count();
            set_ordepool_count(tx_count, bundle_count);
            debug!(
                block_number,
                tx_count,
                bundle_count,
                update_time_ms = update_time.as_millis(),
                "Cleaned orderpool",
            );
        }

        global_cancellation.cancel();
        info!("Clean orderpool job: finished");
    });
    Ok(handle)
}
