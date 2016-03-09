// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use std::collections::HashMap;
use std::mem;

use error::{ClientError, InternalError};
use lru_time_cache::LruCache;
use maidsafe_utilities::serialisation;
use routing::{Authority, Data, MessageId, RequestContent, RequestMessage};
use sodiumoxide::crypto::hash::sha512;
use time::Duration;
use types::{Refresh, RefreshValue};
use utils;
use vault::RoutingNode;
use xor_name::XorName;

const DEFAULT_ACCOUNT_SIZE: u64 = 1_073_741_824;  // 1 GB
const DEFAULT_PAYMENT: u64 = 1_048_576;  // 1 MB

#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
pub struct Account {
    data_stored: u64,
    space_available: u64,
}

impl Default for Account {
    fn default() -> Account {
        Account {
            data_stored: 0,
            space_available: DEFAULT_ACCOUNT_SIZE,
        }
    }
}

impl Account {
    fn put_data(&mut self, size: u64) -> Result<(), ClientError> {
        if size > self.space_available {
            return Err(ClientError::LowBalance);
        }
        self.data_stored += size;
        self.space_available -= size;
        Ok(())
    }

    fn delete_data(&mut self, size: u64) {
        if self.data_stored < size {
            self.space_available += self.data_stored;
            self.data_stored = 0;
        } else {
            self.data_stored -= size;
            self.space_available += size;
        }
    }
}



pub struct MaidManager {
    accounts: HashMap<XorName, Account>,
    request_cache: LruCache<MessageId, RequestMessage>,
}

impl MaidManager {
    pub fn new() -> MaidManager {
        MaidManager {
            accounts: HashMap::new(),
            request_cache: LruCache::with_expiry_duration_and_capacity(Duration::minutes(5), 1000),
        }
    }

    pub fn handle_put(&mut self,
                      routing_node: &RoutingNode,
                      request: &RequestMessage)
                      -> Result<(), InternalError> {
        match request.content {
            RequestContent::Put(Data::Immutable(_), _) => {
                self.handle_put_immutable_data(routing_node, request)
            }
            RequestContent::Put(Data::Structured(_), _) => {
                self.handle_put_structured_data(routing_node, request)
            }
            _ => unreachable!("Error in vault demuxing"),
        }
    }

    pub fn handle_put_success(&mut self,
                              routing_node: &RoutingNode,
                              message_id: &MessageId)
                              -> Result<(), InternalError> {
        match self.request_cache.remove(message_id) {
            Some(client_request) => {
                // Send success response back to client
                let message_hash =
                    sha512::hash(&try!(serialisation::serialise(&client_request))[..]);
                let src = client_request.dst;
                let dst = client_request.src;
                let _ = routing_node.send_put_success(src, dst, message_hash, *message_id);
                Ok(())
            }
            None => Err(InternalError::FailedToFindCachedRequest(*message_id)),
        }
    }

    pub fn handle_put_failure(&mut self,
                              routing_node: &RoutingNode,
                              message_id: &MessageId,
                              external_error_indicator: &[u8])
                              -> Result<(), InternalError> {
        match self.request_cache.remove(message_id) {
            Some(client_request) => {
                // Refund account
                match self.accounts.get_mut(client_request.dst.name()) {
                    Some(account) => {
                        account.delete_data(DEFAULT_PAYMENT /* data.payload_size() as u64 */)
                    }
                    None => return Ok(()),
                }

                // Send failure response back to client
                let error =
                    try!(serialisation::deserialise::<ClientError>(external_error_indicator));
                self.reply_with_put_failure(routing_node, client_request, *message_id, &error)
            }
            None => Err(InternalError::FailedToFindCachedRequest(*message_id)),
        }
    }

    pub fn handle_refresh(&mut self, name: XorName, account: Account) {
        let _ = self.accounts.insert(name, account);
    }

    pub fn handle_churn(&mut self, routing_node: &RoutingNode) {
        // Only retain accounts for which we're still in the close group
        let accounts = mem::replace(&mut self.accounts, HashMap::new());
        self.accounts = accounts.into_iter()
                                .filter(|&(ref maid_name, ref account)| {
                                    match routing_node.close_group(*maid_name) {
                                        Ok(None) => {
                                            trace!("No longer a MM for {}", maid_name);
                                            false
                                        }
                                        Ok(Some(_)) => {
                                            self.send_refresh(routing_node, maid_name, account);
                                            true
                                        }
                                        Err(error) => {
                                            error!("Failed to get close group: {:?} for {}",
                                                   error,
                                                   maid_name);
                                            false
                                        }
                                    }
                                })
                                .collect();
    }

    fn send_refresh(&self, routing_node: &RoutingNode, maid_name: &XorName, account: &Account) {
        let src = Authority::ClientManager(*maid_name);
        let refresh = Refresh::new(maid_name, RefreshValue::MaidManagerAccount(account.clone()));
        if let Ok(serialised_refresh) = serialisation::serialise(&refresh) {
            trace!("MaidManager sending refresh for account {}", src.name());
            let _ = routing_node.send_refresh_request(src, serialised_refresh);
        }
    }

    fn handle_put_immutable_data(&mut self,
                                 routing_node: &RoutingNode,
                                 request: &RequestMessage)
                                 -> Result<(), InternalError> {
        let (data, message_id) = if let RequestContent::Put(Data::Immutable(ref data),
                                                            ref message_id) = request.content {
            (Data::Immutable(data.clone()), message_id)
        } else {
            unreachable!("Logic error")
        };
        let client_name = utils::client_name(&request.src);
        trace!("MM received put request of data {} from client {}", data.name(), client_name);
        self.forward_put_request(routing_node, client_name, data, *message_id, request)
    }

    fn handle_put_structured_data(&mut self,
                                  routing_node: &RoutingNode,
                                  request: &RequestMessage)
                                  -> Result<(), InternalError> {
        let (data, type_tag, message_id) = if let RequestContent::Put(Data::Structured(ref data),
                                                                      ref message_id) =
                                                  request.content {
            (Data::Structured(data.clone()),
             data.get_type_tag(),
             message_id)
        } else {
            unreachable!("Logic error")
        };

        // If the type_tag is 0, the account must not exist, else it must exist.
        let client_name = utils::client_name(&request.src);
        if type_tag == 0 {
            if self.accounts.contains_key(&client_name) {
                let error = ClientError::AccountExists;
                try!(self.reply_with_put_failure(routing_node,
                                                 request.clone(),
                                                 *message_id,
                                                 &error));
                return Err(InternalError::Client(error));
            }

            // Create the account, the SD incurs charge later on
            let _ = self.accounts.insert(client_name, Account::default());
        }
        self.forward_put_request(routing_node, client_name, data, *message_id, request)
    }

    fn forward_put_request(&mut self,
                           routing_node: &RoutingNode,
                           client_name: XorName,
                           data: Data,
                           message_id: MessageId,
                           request: &RequestMessage)
                           -> Result<(), InternalError> {
        // Account must already exist to Put Data.
        let result = self.accounts
                         .get_mut(&client_name)
                         .ok_or(ClientError::NoSuchAccount)
                         .and_then(|account| {
                             account.put_data(DEFAULT_PAYMENT /* data.payload_size() as u64 */)
                         });
        if let Err(error) = result {
            trace!("MM responds put_failure of data {}, due to error {:?}", data.name(), error);
            try!(self.reply_with_put_failure(routing_node, request.clone(), message_id, &error));
            return Err(InternalError::Client(error));
        }

        {
            // forwarding data_request to NAE Manager
            let src = request.dst.clone();
            let dst = Authority::NaeManager(data.name());
            trace!("MM forwarding put request to {:?}", dst);
            let _ = routing_node.send_put_request(src, dst, data, message_id);
        }

        if let Some(prior_request) = self.request_cache
                                         .insert(message_id, request.clone()) {
            error!("Overwrote existing cached request: {:?}", prior_request);
        }
        Ok(())
    }

    fn reply_with_put_failure(&self,
                              routing_node: &RoutingNode,
                              request: RequestMessage,
                              message_id: MessageId,
                              error: &ClientError)
                              -> Result<(), InternalError> {
        let src = request.dst.clone();
        let dst = request.src.clone();
        let external_error_indicator = try!(serialisation::serialise(error));
        let _ = routing_node.send_put_failure(src,
                                              dst,
                                              request,
                                              external_error_indicator,
                                              message_id);
        Ok(())
    }
}


#[cfg(all(test, feature = "use-mock-routing"))]
mod test {
    use super::*;
    use error::{ClientError, InternalError};
    use maidsafe_utilities::serialisation;
    use rand::random;
    use routing::{Authority, Data, ImmutableData, ImmutableDataType, MessageId, RequestContent,
                  RequestMessage, ResponseContent};
    use sodiumoxide::crypto::sign;
    use std::sync::mpsc;
    use utils::generate_random_vec_u8;
    use vault::RoutingNode;
    use xor_name::XorName;

    struct Environment {
        our_authority: Authority,
        client: Authority,
        routing: RoutingNode,
        maid_manager: MaidManager,
    }

    fn environment_setup() -> Environment {
        let from = random::<XorName>();
        let keys = sign::gen_keypair();
        Environment {
            our_authority: Authority::ClientManager(from),
            client: Authority::Client {
                client_key: keys.0,
                peer_id: random(),
                proxy_node_name: from,
            },
            routing: unwrap_result!(RoutingNode::new(mpsc::channel().0)),
            maid_manager: MaidManager::new(),
        }
    }

    #[test]
    fn handle_put_without_account() {
        let mut env = environment_setup();

        // Try with valid ImmutableData before account is created
        let immutable_data = ImmutableData::new(ImmutableDataType::Normal,
                                                generate_random_vec_u8(1024));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Immutable(immutable_data), message_id),
        };

        if let Err(InternalError::Client(ClientError::NoSuchAccount)) =
               env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }
        let put_requests = env.routing.put_requests_given();
        assert!(put_requests.is_empty());
        let put_failures = env.routing.put_failures_given();
        assert_eq!(put_failures.len(), 1);
        assert_eq!(put_failures[0].src, env.our_authority);
        assert_eq!(put_failures[0].dst, env.client);
        if let ResponseContent::PutFailure{ ref id, ref request, ref external_error_indicator } =
               put_failures[0].content {
            assert_eq!(*id, message_id);
            assert_eq!(*request, valid_request);
            if let ClientError::NoSuchAccount =
                   unwrap_result!(serialisation::deserialise(external_error_indicator)) {} else {
                unreachable!()
            }
        } else {
            unreachable!()
        }

        // assert_eq!(::utils::HANDLED,
        //            maid_manager.handle_put(&our_authority,
        //                                    &client,
        //                                    &::routing::data::Data::Immutable(data.clone()),
        //                                    &None));
        // let put_requests = routing.put_requests_given();
        // assert_eq!(put_requests.len(), 1);
        // assert_eq!(put_requests[0].our_authority, our_authority);
        // assert_eq!(put_requests[0].location, Authority::NaeManager(data.name()));
        // assert_eq!(put_requests[0].data, Data::Immutable(data));
    }

    // #[test]
    // fn handle_churn_and_account_transfer() {
    //     let churn_node = random();
    //     let (our_authority, routing, mut maid_manager, client, data) = env_setup();
    //     assert_eq!(::utils::HANDLED,
    //                maid_manager.handle_put(&our_authority,
    //                                        &client,
    //                                        &::routing::data::Data::Immutable(data.clone()),
    //                                        &None));
    //     maid_manager.handle_churn(&churn_node);
    //     let refresh_requests = routing.refresh_requests_given();
    //     assert_eq!(refresh_requests.len(), 1);
    //     assert_eq!(refresh_requests[0].type_tag, ACCOUNT_TAG);
    //     assert_eq!(refresh_requests[0].our_authority.name(),
    //                client.name());

    //     let mut d = ::cbor::Decoder::from_bytes(&refresh_requests[0].content[..]);
    //     if let Some(mm_account) = d.decode().next().and_then(|result| result.ok()) {
    //         maid_manager.database.handle_account_transfer(mm_account);
    //     }
    //     maid_manager.handle_churn(&churn_node);
    //     let refresh_requests = routing.refresh_requests_given();
    //     assert_eq!(refresh_requests.len(), 2);
    //     assert_eq!(refresh_requests[0], refresh_requests[1]);
    // }
}
