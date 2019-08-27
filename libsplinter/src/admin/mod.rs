// Copyright 2019 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod consensus;
mod error;
pub mod messages;
mod shared;

use std::fmt::Write;
use std::sync::{Arc, Mutex, RwLock};

use openssl::hash::{hash, MessageDigest};
use protobuf::{self, Message};

use crate::actix_web::{web, Error as ActixError, HttpRequest, HttpResponse};
use crate::circuit::SplinterState;
use crate::consensus::{Proposal, ProposalUpdate};
use crate::futures::{Future, IntoFuture};
use crate::network::{
    auth::{AuthorizationCallbackError, AuthorizationInquisitor, PeerAuthorizationState},
    peer::PeerConnector,
};
use crate::orchestrator::ServiceOrchestrator;
use crate::protos::admin::{AdminMessage, AdminMessage_Type};
use crate::rest_api::{Method, Request, Resource, RestResourceProvider};
use crate::service::{
    error::{ServiceDestroyError, ServiceError, ServiceStartError, ServiceStopError},
    Service, ServiceMessageContext, ServiceNetworkRegistry,
};

use self::consensus::AdminConsensusManager;
use self::error::{AdminError, Sha256Error};
use self::messages::{from_payload, CircuitProposalVote, CreateCircuit};
use self::shared::AdminServiceShared;

pub struct AdminService {
    service_id: String,
    admin_service_shared: Arc<Mutex<AdminServiceShared>>,
    consensus: Option<AdminConsensusManager>,
}

impl AdminService {
    pub fn new(
        node_id: &str,
        orchestrator: ServiceOrchestrator,
        peer_connector: PeerConnector,
        authorization_inquistor: Box<dyn AuthorizationInquisitor>,
        splinter_state: Arc<RwLock<SplinterState>>,
    ) -> Result<Self, ServiceError> {
        let new_service = Self {
            service_id: admin_service_id(node_id),
            admin_service_shared: Arc::new(Mutex::new(AdminServiceShared::new(
                node_id.to_string(),
                orchestrator,
                peer_connector,
                authorization_inquistor,
                splinter_state,
            ))),
            consensus: None,
        };

        let auth_callback_shared = Arc::clone(&new_service.admin_service_shared);

        new_service
            .admin_service_shared
            .lock()
            .map_err(|_| {
                ServiceError::PoisonedLock(
                    "The lock was poisoned while creating the service".into(),
                )
            })?
            .auth_inquisitor()
            .register_callback(Box::new(
                move |peer_id: &str, state: PeerAuthorizationState| {
                    auth_callback_shared
                        .lock()
                        .map_err(|_| {
                            AuthorizationCallbackError(
                                "admin service shared lock was poisoned".into(),
                            )
                        })?
                        .on_authorization_change(peer_id, state);

                    Ok(())
                },
            ))
            .map_err(|err| ServiceError::UnableToCreate(Box::new(err)))?;

        Ok(new_service)
    }
}

impl Service for AdminService {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn service_type(&self) -> &str {
        "admin"
    }

    fn start(
        &mut self,
        service_registry: &dyn ServiceNetworkRegistry,
    ) -> Result<(), ServiceStartError> {
        if self.consensus.is_some() {
            return Err(ServiceStartError::AlreadyStarted);
        }

        let network_sender = service_registry.connect(&self.service_id)?;

        {
            let mut admin_service_shared = self.admin_service_shared.lock().map_err(|_| {
                ServiceStartError::PoisonedLock("the admin shared lock was poisoned".into())
            })?;

            admin_service_shared.set_network_sender(Some(network_sender));
        }

        // Setup consensus
        self.consensus = Some(
            AdminConsensusManager::new(self.service_id().into(), self.admin_service_shared.clone())
                .map_err(|err| ServiceStartError::Internal(Box::new(err)))?,
        );
        Ok(())
    }

    fn stop(
        &mut self,
        service_registry: &dyn ServiceNetworkRegistry,
    ) -> Result<(), ServiceStopError> {
        service_registry.disconnect(&self.service_id)?;

        let mut admin_service_shared = self.admin_service_shared.lock().map_err(|_| {
            ServiceStopError::PoisonedLock("the admin shared lock was poisoned".into())
        })?;

        // Shutdown consensus
        self.consensus
            .take()
            .ok_or_else(|| ServiceStopError::NotStarted)?
            .shutdown()
            .map_err(|err| ServiceStopError::Internal(Box::new(err)))?;

        // Disconnect from splinter network
        service_registry.disconnect(&self.service_id)?;

        admin_service_shared.set_network_sender(None);

        info!("Admin service stopped and disconnected");

        Ok(())
    }

    fn destroy(self: Box<Self>) -> Result<(), ServiceDestroyError> {
        if self.consensus.is_some() {
            Err(ServiceDestroyError::NotStopped)
        } else {
            Ok(())
        }
    }

    fn handle_message(
        &self,
        message_bytes: &[u8],
        message_context: &ServiceMessageContext,
    ) -> Result<(), ServiceError> {
        let admin_message: AdminMessage = protobuf::parse_from_bytes(message_bytes)
            .map_err(|err| ServiceError::InvalidMessageFormat(Box::new(err)))?;
        debug!("received admin message {:?}", admin_message);
        match admin_message.get_message_type() {
            AdminMessage_Type::CONSENSUS_MESSAGE => self
                .consensus
                .as_ref()
                .ok_or_else(|| ServiceError::NotStarted)?
                .handle_message(admin_message.get_consensus_message())
                .map_err(|err| ServiceError::UnableToHandleMessage(Box::new(err))),
            AdminMessage_Type::PROPOSED_CIRCUIT => {
                let proposed_circuit = admin_message.get_proposed_circuit();

                let expected_hash = proposed_circuit.get_expected_hash().to_vec();
                let circuit_payload = proposed_circuit.get_circuit_payload();
                let required_verifiers = proposed_circuit.get_required_verifiers();
                let mut proposal = Proposal::default();

                proposal.id = sha256(circuit_payload)
                    .map_err(|err| ServiceError::UnableToHandleMessage(Box::new(err)))?
                    .as_bytes()
                    .into();
                proposal.summary = expected_hash;
                proposal.consensus_data = required_verifiers.to_vec();

                let mut admin_service_shared = self.admin_service_shared.lock().map_err(|_| {
                    ServiceError::PoisonedLock("the admin shared lock was poisoned".into())
                })?;

                admin_service_shared.add_pending_consesus_proposal(
                    proposal.id.clone(),
                    (proposal.clone(), circuit_payload.clone()),
                );

                self.consensus
                    .as_ref()
                    .ok_or_else(|| ServiceError::NotStarted)?
                    .send_update(ProposalUpdate::ProposalReceived(
                        proposal,
                        message_context.sender.as_bytes().into(),
                    ))
                    .map_err(|err| ServiceError::UnableToHandleMessage(Box::new(err)))
            }
            AdminMessage_Type::UNSET => Err(ServiceError::InvalidMessageFormat(Box::new(
                AdminError::MessageTypeUnset,
            ))),
        }
    }
}

pub fn admin_service_id(node_id: &str) -> String {
    format!("admin::{}", node_id)
}

pub fn sha256<T>(message: &T) -> Result<String, Sha256Error>
where
    T: Message,
{
    let bytes = message
        .write_to_bytes()
        .map_err(|err| Sha256Error(Box::new(err)))?;
    hash(MessageDigest::sha256(), &bytes)
        .map(|digest| to_hex(&*digest))
        .map_err(|err| Sha256Error(Box::new(err)))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut buf = String::new();
    for b in bytes {
        write!(&mut buf, "{:0x}", b).expect("Unable to write to string");
    }

    buf
}

impl RestResourceProvider for AdminService {
    fn resources(&self) -> Vec<Resource> {
        vec![
            make_create_circuit_route(self.admin_service_shared.clone()),
            make_application_handler_registration_route(self.admin_service_shared.clone()),
            make_vote_route(self.admin_service_shared.clone()),
        ]
    }
}

fn make_create_circuit_route(shared: Arc<Mutex<AdminServiceShared>>) -> Resource {
    Resource::new(Method::Post, "/admin/circuit", move |request, payload| {
        create_circuit(request, payload, shared.clone())
    })
}

fn make_vote_route(shared: Arc<Mutex<AdminServiceShared>>) -> Resource {
    Resource::new(Method::Post, "/admin/vote", move |_, payload| {
        Box::new(
            from_payload::<CircuitProposalVote>(payload).and_then(|vote| {
                debug!("Received vote {:#?}", vote);
                HttpResponse::Accepted().finish().into_future()
            }),
        )
    })
}

fn make_application_handler_registration_route(shared: Arc<Mutex<AdminServiceShared>>) -> Resource {
    Resource::new(
        Method::Get,
        "/ws/admin/register/{type}",
        move |request, payload| {
            let circuit_management_type = if let Some(t) = request.match_info().get("type") {
                t.to_string()
            } else {
                return Box::new(HttpResponse::BadRequest().finish().into_future());
            };

            let unlocked_shared = shared.lock();

            match unlocked_shared {
                Ok(mut shared) => {
                    let request = Request::from((request, payload));
                    debug!("circuit management type {}", circuit_management_type);
                    match shared.add_subscriber(circuit_management_type, request) {
                        Ok(res) => {
                            debug!("Websocket response: {:?}", res);
                            Box::new(res.into_future())
                        }
                        Err(err) => {
                            debug!("Failed to create websocket: {:?}", err);
                            Box::new(HttpResponse::InternalServerError().finish().into_future())
                        }
                    }
                }
                Err(err) => {
                    debug!("Failed to add socket sender: {:?}", err);
                    Box::new(HttpResponse::InternalServerError().finish().into_future())
                }
            }
        },
    )
}

fn create_circuit(
    _req: HttpRequest,
    payload: web::Payload,
    shared: Arc<Mutex<AdminServiceShared>>,
) -> Box<dyn Future<Item = HttpResponse, Error = ActixError>> {
    Box::new(
        from_payload::<CreateCircuit>(payload).and_then(move |create_circuit| {
            let mut circuit_create_request = match create_circuit.into_proto() {
                Ok(request) => request,
                Err(_) => return Ok(HttpResponse::BadRequest().finish()),
            };
            let circuit = circuit_create_request.take_circuit();
            let circuit_id = circuit.circuit_id.clone();
            let mut shared = shared.lock().expect("the admin state lock was poisoned");
            if let Err(err) = shared.propose_circuit(circuit) {
                error!("Unable to submit circuit {} proposal: {}", circuit_id, err);
                Ok(HttpResponse::BadRequest().finish())
            } else {
                debug!("Circuit {} proposed", circuit_id);
                Ok(HttpResponse::Accepted().finish())
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Sender};
    use std::time::{Duration, Instant};
    use tempdir::TempDir;

    use crate::circuit::{directory::CircuitDirectory, SplinterState};
    use crate::mesh::Mesh;
    use crate::network::{auth::AuthorizationCallback, Network};
    use crate::protos::admin;
    use crate::service::{error, ServiceNetworkRegistry, ServiceNetworkSender};
    use crate::storage::get_storage;
    use crate::transport::inproc::InprocTransport;
    use crate::transport::{
        ConnectError, Connection, DisconnectError, RecvError, SendError, Transport,
    };

    /// Test that a circuit creation creates the correct connections and sends the appropriate
    /// messages.
    #[test]
    fn test_propose_circuit() {
        let mesh = Mesh::new(4, 16);
        let network = Network::new(mesh.clone());
        let transport =
            MockConnectingTransport::expect_connections(vec![Ok(Box::new(MockConnection))]);

        // create temp directory
        let temp_dir = TempDir::new("test_circuit_write_file").unwrap();
        let temp_dir = temp_dir.path().to_path_buf();

        // setup empty state filename
        let path = setup_storage(temp_dir);
        let mut storage = get_storage(&path, CircuitDirectory::new).unwrap();
        let circuit_directory = storage.write().clone();
        let state = Arc::new(RwLock::new(SplinterState::new(
            path.to_string(),
            circuit_directory,
        )));
        let orchestrator =
            ServiceOrchestrator::new(vec![], "".to_string(), InprocTransport::default());
        let peer_connector = PeerConnector::new(network.clone(), Box::new(transport));
        let mut admin_service = AdminService::new(
            "test-node".into(),
            orchestrator,
            peer_connector,
            Box::new(MockAuthInquisitor),
            state,
        )
        .expect("Service should have been created correctly");

        let (tx, rx) = channel();
        admin_service
            .start(&MockNetworkRegistry { tx })
            .expect("Service should have started correctly");

        let mut proposed_circuit = admin::Circuit::new();
        proposed_circuit.set_circuit_id("test_propose_circuit".into());
        proposed_circuit
            .set_authorization_type(admin::Circuit_AuthorizationType::TRUST_AUTHORIZATION);
        proposed_circuit.set_persistence(admin::Circuit_PersistenceType::ANY_PERSISTENCE);
        proposed_circuit.set_routes(admin::Circuit_RouteType::ANY_ROUTE);
        proposed_circuit.set_durability(admin::Circuit_DurabilityType::NO_DURABILITY);
        proposed_circuit.set_circuit_management_type("test app auth handler".into());

        proposed_circuit.set_members(protobuf::RepeatedField::from_vec(vec![
            splinter_node("test-node", "tcp://someplace:8000"),
            splinter_node("other-node", "tcp://otherplace:8000"),
        ]));
        proposed_circuit.set_roster(protobuf::RepeatedField::from_vec(vec![
            splinter_service("service-a", "sabre"),
            splinter_service("service-b", "sabre"),
        ]));

        admin_service
            .admin_service_shared
            .lock()
            .unwrap()
            .propose_circuit(proposed_circuit.clone())
            .expect("The proposal was not handled correctly");

        // wait up to 1 second for the proposed circuit message
        let recipient;
        let message;
        let start = Instant::now();
        loop {
            if Instant::now().duration_since(start) > Duration::from_secs(1) {
                panic!("Failed to receive proposed circuit message in time");
            }
            if let Ok((r, m)) = rx.recv_timeout(Duration::from_millis(100)) {
                recipient = r;
                message = m;
                break;
            }
        }

        assert_eq!("admin::other-node".to_string(), recipient);

        let mut admin_envelope: admin::AdminMessage =
            protobuf::parse_from_bytes(&message).expect("The message could not be parsed");

        assert_eq!(
            admin::AdminMessage_Type::PROPOSED_CIRCUIT,
            admin_envelope.get_message_type()
        );

        let mut envelope = admin_envelope
            .take_proposed_circuit()
            .take_circuit_payload();
        assert_eq!(
            admin::CircuitManagementPayload_Action::CIRCUIT_CREATE_REQUEST,
            envelope.get_action()
        );
        assert_eq!(
            proposed_circuit,
            envelope.take_circuit_create_request().take_circuit()
        );
    }

    fn splinter_node(node_id: &str, endpoint: &str) -> admin::SplinterNode {
        let mut node = admin::SplinterNode::new();
        node.set_node_id(node_id.into());
        node.set_endpoint(endpoint.into());
        node
    }

    fn splinter_service(service_id: &str, service_type: &str) -> admin::SplinterService {
        let mut service = admin::SplinterService::new();
        service.set_service_id(service_id.into());
        service.set_service_type(service_type.into());
        service
    }

    struct MockNetworkRegistry {
        tx: Sender<(String, Vec<u8>)>,
    }

    impl ServiceNetworkRegistry for MockNetworkRegistry {
        fn connect(
            &self,
            _service_id: &str,
        ) -> Result<Box<dyn ServiceNetworkSender>, error::ServiceConnectionError> {
            Ok(Box::new(MockNetworkSender {
                tx: self.tx.clone(),
            }))
        }

        fn disconnect(&self, _service_id: &str) -> Result<(), error::ServiceDisconnectionError> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct MockNetworkSender {
        tx: Sender<(String, Vec<u8>)>,
    }

    impl ServiceNetworkSender for MockNetworkSender {
        fn send(&self, recipient: &str, message: &[u8]) -> Result<(), error::ServiceSendError> {
            self.tx
                .send((recipient.to_string(), message.to_vec()))
                .expect("Unable to send test message");

            Ok(())
        }

        fn send_and_await(
            &self,
            _recipient: &str,
            _message: &[u8],
        ) -> Result<Vec<u8>, error::ServiceSendError> {
            panic!("MockNetworkSender.send_and_await unexpectedly called")
        }

        fn reply(
            &self,
            _message_origin: &ServiceMessageContext,
            _message: &[u8],
        ) -> Result<(), error::ServiceSendError> {
            panic!("MockNetworkSender.reply unexpectedly called")
        }

        fn clone_box(&self) -> Box<dyn ServiceNetworkSender> {
            Box::new(self.clone())
        }
    }

    struct MockConnectingTransport {
        connection_results: VecDeque<Result<Box<dyn Connection>, ConnectError>>,
    }

    impl MockConnectingTransport {
        fn expect_connections(results: Vec<Result<Box<dyn Connection>, ConnectError>>) -> Self {
            Self {
                connection_results: results.into_iter().collect(),
            }
        }
    }

    impl Transport for MockConnectingTransport {
        fn accepts(&self, _: &str) -> bool {
            true
        }

        fn connect(&mut self, _: &str) -> Result<Box<dyn Connection>, ConnectError> {
            self.connection_results
                .pop_front()
                .expect("No test result added to mock")
        }

        fn listen(
            &mut self,
            _: &str,
        ) -> Result<Box<dyn crate::transport::Listener>, crate::transport::ListenError> {
            panic!("MockConnectingTransport.listen unexpectedly called")
        }
    }

    struct MockConnection;

    impl Connection for MockConnection {
        fn send(&mut self, _message: &[u8]) -> Result<(), SendError> {
            Ok(())
        }

        fn recv(&mut self) -> Result<Vec<u8>, RecvError> {
            panic!("MockConnection.recv unexpectedly called")
        }

        fn remote_endpoint(&self) -> String {
            String::from("MockConnection")
        }

        fn local_endpoint(&self) -> String {
            String::from("MockConnection")
        }

        fn disconnect(&mut self) -> Result<(), DisconnectError> {
            Ok(())
        }

        fn evented(&self) -> &dyn mio::Evented {
            &MockEvented
        }
    }

    struct MockEvented;

    impl mio::Evented for MockEvented {
        fn register(
            &self,
            _poll: &mio::Poll,
            _token: mio::Token,
            _interest: mio::Ready,
            _opts: mio::PollOpt,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn reregister(
            &self,
            _poll: &mio::Poll,
            _token: mio::Token,
            _interest: mio::Ready,
            _opts: mio::PollOpt,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn deregister(&self, _poll: &mio::Poll) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct MockAuthInquisitor;

    impl AuthorizationInquisitor for MockAuthInquisitor {
        fn is_authorized(&self, _: &str) -> bool {
            true
        }

        fn register_callback(
            &self,
            _: Box<dyn AuthorizationCallback>,
        ) -> Result<(), AuthorizationCallbackError> {
            // The callback won't be called, as this test implementation indicates that all nodes
            // are peered.
            Ok(())
        }
    }

    fn setup_storage(mut temp_dir: PathBuf) -> String {
        // Creat the temp file
        temp_dir.push("circuits.yaml");
        let path = temp_dir.to_str().unwrap().to_string();

        // Write out the mock state file to the temp directory
        path
    }
}
