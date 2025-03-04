use std::collections::HashSet;

use anyhow::Result;
use enumflags2::BitFlags;
use tracing::trace;
use zbus::{
    dbus_interface,
    fdo::{self, ReleaseNameReply, RequestNameFlags, RequestNameReply},
    names::{BusName, OwnedBusName, OwnedUniqueName, OwnedWellKnownName},
    AuthMechanism, Connection, ConnectionBuilder, Guid, MessageStream, OwnedMatchRule, Socket,
};

use crate::name_registry::NameRegistry;

/// A peer connection.
#[derive(Debug)]
pub struct Peer {
    conn: Connection,
    unique_name: OwnedUniqueName,
}

impl Peer {
    pub async fn new(
        guid: &Guid,
        id: usize,
        socket: Box<dyn Socket + 'static>,
        name_registry: NameRegistry,
        auth_mechanism: AuthMechanism,
    ) -> Result<Self> {
        let unique_name = OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap();

        let conn = ConnectionBuilder::socket(socket)
            .server(guid)
            .p2p()
            .serve_at(
                "/org/freedesktop/DBus",
                DBus::new(unique_name.clone(), name_registry),
            )?
            .name("org.freedesktop.DBus")?
            .unique_name("org.freedesktop.DBus")?
            .auth_mechanisms(&[auth_mechanism])
            .build()
            .await?;
        trace!("created: {:?}", conn);

        Ok(Self { conn, unique_name })
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn stream(&self) -> MessageStream {
        MessageStream::from(&self.conn)
    }

    /// # Panics
    ///
    /// if header, SENDER or DESTINATION is not set.
    pub async fn interested(&self, msg: &zbus::Message) -> bool {
        let dbus_ref = self
            .conn
            .object_server()
            .interface::<_, DBus>("/org/freedesktop/DBus")
            .await
            .expect("DBus interface not found");
        let dbus = dbus_ref.get().await;
        let hdr = msg.header().expect("received message without header");

        dbus.match_rules.iter().any(|rule| {
            // First make use of zbus API
            match rule.matches(msg) {
                Ok(false) => return false,
                Ok(true) => (),
                Err(e) => {
                    tracing::warn!("error matching rule: {}", e);

                    return false;
                }
            }

            // Then match sender and destination involving well-known names, manually.
            if let Some(sender) = rule.sender().cloned().and_then(|name| match name {
                BusName::WellKnown(name) => dbus.name_registry.lookup(name).as_deref().cloned(),
                // Unique name is already taken care of by the zbus API.
                BusName::Unique(_) => None,
            }) {
                if sender
                    != hdr
                        .sender()
                        .expect("SENDER field unset")
                        .expect("SENDER field unset")
                        .clone()
                {
                    return false;
                }
            }

            // The destination.
            if let Some(destination) = rule.destination() {
                match hdr
                    .destination()
                    .expect("DESTINATION field unset")
                    .expect("DESTINATION field unset")
                    .clone()
                {
                    BusName::WellKnown(name) => match dbus.name_registry.lookup(name) {
                        Some(name) if name == *destination => (),
                        Some(_) => return false,
                        None => return false,
                    },
                    // Unique name is already taken care of by the zbus API.
                    BusName::Unique(_) => {}
                }
            }

            true
        })
    }
}

#[derive(Debug)]
struct DBus {
    greeted: bool,
    unique_name: OwnedUniqueName,
    name_registry: NameRegistry,
    match_rules: HashSet<OwnedMatchRule>,
}

impl DBus {
    fn new(unique_name: OwnedUniqueName, name_registry: NameRegistry) -> Self {
        Self {
            greeted: false,
            unique_name,
            name_registry,
            match_rules: HashSet::new(),
        }
    }
}

#[dbus_interface(interface = "org.freedesktop.DBus")]
impl DBus {
    /// Returns the unique name assigned to the connection.
    async fn hello(&mut self) -> fdo::Result<OwnedUniqueName> {
        if self.greeted {
            return Err(fdo::Error::Failed(
                "Can only call `Hello` method once".to_string(),
            ));
        }
        self.greeted = true;

        Ok(self.unique_name.clone())
    }

    /// Ask the message bus to assign the given name to the method caller.
    fn request_name(
        &self,
        name: OwnedWellKnownName,
        flags: BitFlags<RequestNameFlags>,
    ) -> RequestNameReply {
        self.name_registry
            .request_name(name, self.unique_name.clone(), flags)
    }

    /// Ask the message bus to release the method caller's claim to the given name.
    fn release_name(&self, name: OwnedWellKnownName) -> ReleaseNameReply {
        self.name_registry
            .release_name(name.into(), (&*self.unique_name).into())
    }

    /// Returns the unique connection name of the primary owner of the name given.
    fn get_name_owner(&self, name: OwnedBusName) -> fdo::Result<OwnedUniqueName> {
        match name.into_inner() {
            BusName::WellKnown(name) => self.name_registry.lookup(name).ok_or_else(|| {
                fdo::Error::NameHasNoOwner("Name is not owned by anyone. Take it!".to_string())
            }),
            // FIXME: Not good enough. We need to check if name is actually owned.
            BusName::Unique(name) => Ok(name.into()),
        }
    }

    /// Adds a match rule to match messages going through the message bus
    fn add_match(&mut self, rule: OwnedMatchRule) {
        self.match_rules.insert(rule);
    }

    /// Removes the first rule that matches.
    fn remove_match(&mut self, rule: OwnedMatchRule) -> fdo::Result<()> {
        if !self.match_rules.remove(&rule) {
            return Err(fdo::Error::MatchRuleNotFound(
                "No such match rule".to_string(),
            ));
        }

        Ok(())
    }
}
