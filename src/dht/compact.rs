use std::net::{Ipv4Addr, SocketAddrV4};

const SOCKET_ADDR_V4_LEN: usize = 6;

const COMPACT_NODE_LEN: usize = 26;

pub(crate) mod values {
    use serde::{
        de::{Deserializer, Error as _, SeqAccess, Visitor},
        ser::{SerializeSeq, Serializer},
    };
    use serde_bytes::{ByteBuf, Bytes};
    use std::{fmt, net::SocketAddrV4};

    pub(crate) fn serialize<S>(addrs: &[SocketAddrV4], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = s.serialize_seq(Some(addrs.len()))?;
        for addr in addrs {
            seq.serialize_element(Bytes::new(&super::encode_socket_addr(addr)))?
        }
        seq.end()
    }

    pub(crate) fn deserialize<'de, D>(d: D) -> Result<Vec<SocketAddrV4>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SocketAddrsVisitor;

        impl<'de> Visitor<'de> for SocketAddrsVisitor {
            type Value = Vec<SocketAddrV4>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "list of byte strings")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut output = Vec::with_capacity(seq.size_hint().unwrap_or(0));

                while let Some(bytes) = seq.next_element::<ByteBuf>()? {
                    let item = super::decode_socket_addr(&bytes)
                        .ok_or_else(|| A::Error::invalid_length(bytes.len(), &self))?;
                    output.push(item);
                }

                Ok(output)
            }
        }

        d.deserialize_seq(SocketAddrsVisitor)
    }
}

/// Serialize/deserialize `Vec` of `NodeHandle` in compact format. Specialized for ipv4 addresses.
pub(crate) mod nodes {
    use serde::{de::Deserializer, ser::Serializer, Deserialize};
    use serde_bytes::ByteBuf;

    use crate::dht::{key::Key, node::Node};

    use super::COMPACT_NODE_LEN;

    pub(crate) fn serialize<S>(nodes: &[Node], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buffer = Vec::with_capacity(nodes.len() * (COMPACT_NODE_LEN));

        for node in nodes {
            let encoded_addr = super::encode_socket_addr(&node.addr);

            buffer.extend(node.id.as_ref());
            buffer.extend(encoded_addr);
        }

        s.serialize_bytes(&buffer)
    }

    pub(crate) fn deserialize<'de, D>(d: D) -> Result<Vec<Node>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let buffer = ByteBuf::deserialize(d)?;
        let chunks = buffer.chunks_exact(COMPACT_NODE_LEN);

        let nodes = chunks
            .filter_map(|chunk| {
                let id = Key::from(&chunk[..20]);
                let addr = super::decode_socket_addr(&chunk[20..])?;

                Some(Node::from_addr(id, addr))
            })
            .collect();

        Ok(nodes)
    }
}

pub fn decode_socket_addr(src: &[u8]) -> Option<SocketAddrV4> {
    if src.len() == SOCKET_ADDR_V4_LEN {
        let addr: [u8; 4] = src.get(..4)?.try_into().ok()?;
        let addr = Ipv4Addr::from(addr);
        let port = u16::from_be_bytes(src.get(4..)?.try_into().ok()?);
        Some(SocketAddrV4::new(addr, port))
    } else {
        None
    }
}

pub fn encode_socket_addr(addr: &SocketAddrV4) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(6);
    buffer.extend(addr.ip().octets().as_ref());
    buffer.extend(addr.port().to_be_bytes().as_ref());
    buffer
}
