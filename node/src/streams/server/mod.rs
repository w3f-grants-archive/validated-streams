use crate::{
	service::FullClient,
	streams::{
		configs::LocalNetworkConfiguration,
		gossip::StreamsGossip,
		proofs::EventProofs,
		services::events::{keyvault::KeyVault, EventService},
	},
};
use local_ip_address::local_ip;
use node_runtime::opaque::Block;
use sc_service::{error::Error as ServiceError, SpawnTaskHandle};
use sc_transaction_pool::{BasicPool, FullChainApi};
use sp_core::H256;
use sp_keystore::CryptoStore;
use sp_runtime::key_types::AURA;
use std::{
	io::{Error, ErrorKind},
	sync::Arc,
	time::Duration,
};
pub use tonic::{transport::Server, Request, Response, Status};
pub use validated_streams::{
	streams_server::{Streams, StreamsServer},
	ValidateEventRequest, ValidateEventResponse,
};

pub mod validated_streams {
	tonic::include_proto!("validated_streams");
}

pub struct ValidatedStreamsNode {
	events_service: Arc<EventService>,
}

#[tonic::async_trait]
impl Streams for ValidatedStreamsNode {
	async fn validate_event(
		&self,
		request: Request<ValidateEventRequest>,
	) -> Result<Response<ValidateEventResponse>, Status> {
		let remote_addr = request
			.remote_addr()
			.ok_or_else(|| Status::aborted("Malformed Request, can't retreive Origin address"))?;
		log::info!("Received a request from {:?}", remote_addr);
		let event = request.into_inner();
		// check that event_id is 32 bytes otherwise H256::from_slice would panic
		if event.event_id.len() == 32 {
			Ok(Response::new(ValidateEventResponse {
				status: self
					.events_service
					.handle_client_request(H256::from_slice(event.event_id.as_slice()))
					.await
					.map_err(|e| Status::aborted(e.to_string()))?,
			}))
		} else {
			Err(Error::new(ErrorKind::Other, "invalid event_id sent".to_string()).into())
		}
	}
}

impl ValidatedStreamsNode {
	/// enables the current node to be a validated streams node by runing the core componenets
	/// which are the EventService, the StreamsGossip and the gRPC server.
	pub fn start(
		spawn_handle: SpawnTaskHandle,
		event_proofs: Arc<dyn EventProofs + Send + Sync>,
		client: Arc<FullClient>,
		keystore: Arc<dyn CryptoStore>,
		tx_pool: Arc<BasicPool<FullChainApi<FullClient, Block>, Block>>,
	) -> Result<(), ServiceError> {
		spawn_handle.spawn_blocking(
			"gRPC server",
			None,
			Self::run(spawn_handle.clone(), event_proofs, client, keystore, tx_pool),
		);
		Ok(())
	}

	pub async fn run(
		spawn_handle: SpawnTaskHandle,
		event_proofs: Arc<dyn EventProofs + Send + Sync>,
		client: Arc<FullClient>,
		keystore: Arc<dyn CryptoStore>,
		tx_pool: Arc<BasicPool<FullChainApi<FullClient, Block>, Block>>,
	) {
		//wait until all keys are created by aura
		tokio::time::sleep(Duration::from_millis(3000)).await;

		let keyvault = {
			if let Ok(x) = KeyVault::new(keystore, client.clone(), AURA).await {
				x
			} else {
				log::info!("node is not a validator");
				return
			}
		};

		let (streams_gossip, streams_gossip_service) = StreamsGossip::create();

		let self_addr = LocalNetworkConfiguration::self_multiaddr();
		let peers = LocalNetworkConfiguration::peers_multiaddrs(self_addr.clone());

		let events_service = Arc::new(
			EventService::new(
				KeyVault::validators_pubkeys(client.clone()),
				event_proofs,
				streams_gossip,
				keyvault,
				tx_pool,
				client,
			)
			.await,
		);

		streams_gossip_service
			.start(spawn_handle, self_addr, peers, events_service.clone())
			.await;

		match tokio::spawn(async move {
			log::info!("Server could be reached at {}", local_ip().unwrap().to_string());
			Server::builder()
				.add_service(StreamsServer::new(ValidatedStreamsNode { events_service }))
				.serve("[::0]:5555".parse().expect("Failed parsing gRPC server Address"))
				.await
		})
		.await
		{
			Ok(_) => (),
			Err(e) => {
				panic!("Failed Creating StreamsServer due to Err: {}", e);
			},
		}
	}
}
