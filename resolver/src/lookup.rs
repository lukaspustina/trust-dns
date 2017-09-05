
// Copyright 2015-2017 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Lookup result from a resolution of ipv4 and ipv6 records with a Resolver.

use std::error::Error;
use std::io;
use std::mem;
use std::slice::Iter;
use std::sync::Arc;

use futures::{Async, future, Future, Poll, task};

use trust_dns::client::{ClientHandle, RetryClientHandle, SecureClientHandle};
use trust_dns::error::ClientError;
use trust_dns::op::{Message, Query};
use trust_dns::rr::{Name, RecordType, RData};

use lru::DnsLru;
use name_server_pool::NameServerPool;

/// Result of a DNS query when querying for any record type supported by the TRust-DNS Client library.
///
/// For IP resolution see LookIp, as it has more features for A and AAAA lookups.
#[derive(Debug, Clone)]
pub struct Lookup {
    rdatas: Arc<Vec<RData>>,
}

impl Lookup {
    pub(crate) fn new(rdatas: Arc<Vec<RData>>) -> Self {
        Lookup { rdatas }
    }

    /// Returns a borrowed iterator of the returned IPs
    pub fn iter(&self) -> LookupIter {
        LookupIter(self.rdatas.iter())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rdatas.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.rdatas.len()
    }

    /// Clones the inner vec, appends the other vec
    pub(crate) fn append(&self, other: Lookup) -> Self {
        let mut rdatas = Vec::with_capacity(self.len() + other.len());
        rdatas.extend_from_slice(&*self.rdatas);
        rdatas.extend_from_slice(&*other.rdatas);

        Self::new(Arc::new(rdatas))
    }
}

/// Borrowed view of set of RDatas returned from a Lookup
pub struct LookupIter<'a>(Iter<'a, RData>);

impl<'a> Iterator for LookupIter<'a> {
    type Item = &'a RData;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Different lookup options for the lookup attempts and validation
#[derive(Clone)]
#[doc(hidden)]
pub enum LookupEither {
    Retry(RetryClientHandle<NameServerPool>),
    Secure(SecureClientHandle<RetryClientHandle<NameServerPool>>),
}

impl ClientHandle for LookupEither {
    fn send(&mut self, message: Message) -> Box<Future<Item = Message, Error = ClientError>> {
        match *self {
            LookupEither::Retry(ref mut c) => c.send(message),
            LookupEither::Secure(ref mut c) => c.send(message),
        }
    }
}

/// The Future returned from ResolverFuture when performing a lookup.
pub type LookupFuture = InnerLookupFuture<LookupEither>;

#[doc(hidden)]
/// The Future returned from ResolverFuture when performing a lookup.
pub struct InnerLookupFuture<C: ClientHandle + 'static> {
    client_cache: DnsLru<C>,
    names: Vec<Name>,
    record_type: RecordType,
    future: Box<Future<Item = Lookup, Error = io::Error>>,
}

impl<C: ClientHandle + 'static> InnerLookupFuture<C> {
    /// Perform a lookup from a name and type to a set of RDatas
    ///
    /// # Arguments
    ///
    /// * `names` - a set of DNS names to attempt to resolve, they will be attempted in queue order, i.e. the first is `names.pop()`. Upon each failure, the next will be attempted.
    /// * `record_type` - type of record being sought
    /// * `client_cache` - cache with a connection to use for performing all lookups
    pub(crate) fn lookup(
        mut names: Vec<Name>,
        record_type: RecordType,
        client_cache: &mut DnsLru<C>,
    ) -> Self {
        let name = names.pop().expect("can not lookup IPs for no names");

        let query = lookup(name, record_type, client_cache);
        InnerLookupFuture {
            client_cache: client_cache.clone(),
            names,
            record_type,
            future: Box::new(query),
        }
    }

    fn next_lookup<F: FnOnce() -> Poll<Lookup, io::Error>>(
        &mut self,
        otherwise: F,
    ) -> Poll<Lookup, io::Error> {
        let name = self.names.pop();
        if let Some(name) = name {
            let query = lookup(name, self.record_type, &mut self.client_cache);

            mem::replace(&mut self.future, Box::new(query));
            // guarantee that we get scheduled for the next turn...
            task::current().notify();
            Ok(Async::NotReady)
        } else {
            otherwise()
        }
    }

    pub(crate) fn error<E: Error>(client_cache: DnsLru<C>, error: E) -> Self {
        return InnerLookupFuture {
            // errors on names don't need to be cheap... i.e. this clone is unfortunate in this case.
            client_cache,
            names: vec![],
            record_type: RecordType::NULL,
            future: Box::new(future::err(
                io::Error::new(io::ErrorKind::Other, format!("{}", error)),
            )),
        };
    }
}

impl<C: ClientHandle + 'static> Future for InnerLookupFuture<C> {
    type Item = Lookup;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.future.poll() {
            Ok(Async::Ready(lookup_ip)) => {
                if lookup_ip.rdatas.len() == 0 {
                    return self.next_lookup(|| Ok(Async::Ready(lookup_ip)));
                } else {
                    return Ok(Async::Ready(lookup_ip));
                }
            }
            p @ Ok(Async::NotReady) => p,
            e @ Err(_) => {
                return self.next_lookup(|| e);
            }
        }
    }
}

/// Queries for the specified record type
fn lookup<C: ClientHandle + 'static>(
    name: Name,
    record_type: RecordType,
    client_cache: &mut DnsLru<C>,
) -> Box<Future<Item = Lookup, Error = io::Error>> {
    client_cache.lookup(Query::query(name, record_type))
}

// TODO: maximum recursion on CNAME, etc, chains...
// struct LookupStack(Vec<Query>);

// impl LookupStack {
//     // pushes the Query onto the stack, and returns a reference. An error will be returned
//     fn push(&mut self, query: Query) -> io::Result<&Query> {
//         if self.0.contains(&query) {
//             return Err(io::Error::new(io::ErrorKind::Other, "circular CNAME or other recursion"));
//         }

//         self.0.push(query);
//         Ok(self.0.last().unwrap())
//     }
// }


#[cfg(test)]
pub mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    use futures::{future, Future};

    use trust_dns::client::ClientHandle;
    use trust_dns::error::*;
    use trust_dns::op::Message;
    use trust_dns::rr::{Name, Record, RData, RecordType};

    use super::*;

    #[derive(Clone)]
    pub struct MockClientHandle {
        messages: Arc<Mutex<Vec<ClientResult<Message>>>>,
    }

    impl ClientHandle for MockClientHandle {
        fn send(&mut self, _: Message) -> Box<Future<Item = Message, Error = ClientError>> {
            Box::new(future::result(
                self.messages.lock().unwrap().pop().unwrap_or(empty()),
            ))
        }
    }

    pub fn v4_message() -> ClientResult<Message> {
        let mut message = Message::new();
        message.insert_answers(vec![
            Record::from_rdata(
                Name::root(),
                86400,
                RecordType::A,
                RData::A(Ipv4Addr::new(127, 0, 0, 1))
            ),
        ]);
        Ok(message)
    }

    pub fn empty() -> ClientResult<Message> {
        Ok(Message::new())
    }

    pub fn error() -> ClientResult<Message> {
        Err(ClientErrorKind::Io.into())
    }

    pub fn mock(messages: Vec<ClientResult<Message>>) -> MockClientHandle {
        MockClientHandle { messages: Arc::new(Mutex::new(messages)) }
    }

    #[test]
    fn test_lookup() {
        assert_eq!(
            lookup(
                Name::root(),
                RecordType::A,
                &mut DnsLru::new(0, mock(vec![v4_message()])),
            ).wait()
                .unwrap()
                .iter()
                .map(|r| r.to_ip_addr().unwrap())
                .collect::<Vec<IpAddr>>(),
            vec![Ipv4Addr::new(127, 0, 0, 1)]
        );
    }

    #[test]
    fn test_error() {
        assert!(
            lookup(
                Name::root(),
                RecordType::A,
                &mut DnsLru::new(0, mock(vec![error()])),
            ).wait()
                .is_err()
        );
    }

    #[test]
    fn test_empty_no_response() {
        assert_eq!(
            lookup(
                Name::root(),
                RecordType::A,
                &mut DnsLru::new(0, mock(vec![empty()])),
            ).wait()
                .unwrap()
                .iter()
                .map(|r| r.to_ip_addr().unwrap())
                .collect::<Vec<IpAddr>>(),
            Vec::<IpAddr>::new()
        );
    }
}