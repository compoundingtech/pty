//! Streaming list projection. Keep the canonical metadata deserializer (including
//! unknown provenance/scope fields), but consume retained payloads without storage.

use std::borrow::Cow;

use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::metadata::SessionMetadata;

pub(super) fn parse(bytes: &[u8]) -> serde_json::Result<SessionMetadata> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let metadata = deserializer.deserialize_map(MetadataVisitor)?;
    deserializer.end()?;
    Ok(metadata)
}

struct MetadataVisitor;

impl<'de> Visitor<'de> for MetadataVisitor {
    type Value = SessionMetadata;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("metadata must be a JSON object")
    }

    fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        SessionMetadata::deserialize(de::value::MapAccessDeserializer::new(Projection(map)))
    }
}

struct Projection<M>(M);

// JSON keys normally borrow the input. Only escaped keys need owned scratch.
struct Key<'a>(Cow<'a, str>);

impl<'de> Deserialize<'de> for Key<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = Key<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a metadata field name")
            }
            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
                Ok(Key(Cow::Borrowed(value)))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Key(Cow::Owned(value.to_owned())))
            }
        }
        deserializer.deserialize_str(KeyVisitor)
    }
}

impl<'de, M: MapAccess<'de>> MapAccess<'de> for Projection<M> {
    type Error = M::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        while let Some(Key(key)) = self.0.next_key::<Key<'de>>()? {
            match key.as_ref() {
                "lastLines" => {
                    self.0.next_value::<Option<DiscardStrings<false>>>()?;
                }
                "extraEnv" | "sessionEnv" => {
                    self.0.next_value::<Option<DiscardStrings<true>>>()?;
                }
                _ => {
                    return match key {
                        Cow::Borrowed(key) => seed.deserialize(key.into_deserializer()),
                        Cow::Owned(key) => seed.deserialize(key.into_deserializer()),
                    }.map(Some);
                }
            }
        }
        Ok(None)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.next_value_seed(seed)
    }
}

// IgnoredAny alone would also accept numbers and objects where the old parser
// required strings. Validate the original payload shapes without owning strings.
struct DiscardString;

impl<'de> Deserialize<'de> for DiscardString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StringVisitor;
        impl<'de> Visitor<'de> for StringVisitor {
            type Value = DiscardString;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(DiscardString)
            }
        }
        deserializer.deserialize_str(StringVisitor)
    }
}

struct DiscardStrings<const MAP: bool>;

impl<'de, const MAP: bool> Deserialize<'de> for DiscardStrings<MAP> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StringsVisitor<const MAP: bool>;
        impl<'de, const MAP: bool> Visitor<'de> for StringsVisitor<MAP> {
            type Value = DiscardStrings<MAP>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(if MAP { "a map" } else { "a sequence" })
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<DiscardString>()?.is_some() {}
                Ok(DiscardStrings)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<DiscardString, DiscardString>()?.is_some() {}
                Ok(DiscardStrings)
            }
        }
        if MAP {
            deserializer.deserialize_map(StringsVisitor::<MAP>)
        } else {
            deserializer.deserialize_seq(StringsVisitor::<MAP>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::hint::black_box;
    use std::time::Instant;

    fn old(bytes: &[u8]) -> serde_json::Result<SessionMetadata> {
        let value: Value = serde_json::from_slice(bytes)?;
        if !value.is_object() {
            return Err(<serde_json::Error as de::Error>::custom("metadata must be a JSON object"));
        }
        serde_json::from_value(value)
    }

    fn fixture() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "generation": "agent/example-generation", "daemonPid": 42,
            "daemonStartToken": "agent/example-token", "daemonBootId": "agent/example-boot",
            "recovery": {"processStartToken": "agent/example-recovery"},
            "command": "sh", "args": ["-c", "true"], "displayCommand": "true",
            "cwd": "/", "createdAt": "2026-10-21T00:00:00.000Z",
            "displayName": "agent/example-session", "tags": {
                "owner": "person/robin", "start-operation": "agent/example-operation",
                "scope": "agent/example-scope", "strategy": "permanent"
            },
            "exitCode": 0, "exitedAt": "2026-10-21T00:00:01.000Z",
            "lastAttachAt": "2026-10-21T00:00:00.000Z", "lastOutputAtMs": 42,
            "clientGeneration": 3, "rows": 24, "cols": 80, "ephemeral": false,
            "isolateEnv": true, "unsetEnv": ["DISPLAY"], "env": {"KEEP": "value"},
            "provenance": {"owner": "person/blair"}, "scope": "agent/example-scope",
            "lastLines": vec!["x".repeat(100); 100],
            "extraEnv": (0..24).map(|i| (format!("KEY_{i}"), Value::String("y".repeat(90)))).collect::<serde_json::Map<_, _>>(),
            "sessionEnv": (0..24).map(|i| (format!("KEY_{i}"), Value::String("z".repeat(90)))).collect::<serde_json::Map<_, _>>()
        })).unwrap()
    }

    #[test]
    fn projection_equivalence() {
        let bytes = fixture();
        let mut expected = old(&bytes).unwrap();
        expected.last_lines = None;
        expected.extra_env = None;
        expected.session_env = None;
        assert_eq!(parse(&bytes).unwrap(), expected);
        // The full metadata reader remains suitable for restart and scrollback.
        assert!(serde_json::from_slice::<SessionMetadata>(&bytes).unwrap().last_lines.is_some());
    }

    #[test]
    fn malformed_and_non_object_equivalence() {
        for bytes in [
            b"{".as_slice(), b"[]", b"null", b"true", b"42", b"\"text\"",
            b"{} trailing", b"{\"args\":null}", b"{\"createdAt\":42}",
            b"{\"lastLines\":[42]}", b"{\"extraEnv\":[]}",
            b"{\"sessionEnv\":{\"KEY\":false}}", b"{\"lastLines\":{}}",
            b"{\"publicationBinding\":{}}", b"{}",
            b"{\"lastLines\":null,\"extraEnv\":null,\"sessionEnv\":null}",
        ] {
            assert_eq!(parse(bytes).is_ok(), old(bytes).is_ok(), "{bytes:?}");
        }
    }

    #[test]
    fn parse_time_report() {
        let sessions = vec![fixture(); 500];
        let start = Instant::now();
        for bytes in &sessions {
            black_box(old(black_box(bytes)).unwrap());
        }
        let old_time = start.elapsed();
        let start = Instant::now();
        for bytes in &sessions {
            black_box(parse(black_box(bytes)).unwrap());
        }
        let new_time = start.elapsed();
        println!("500 sessions, {} bytes/session: old={old_time:?}, new={new_time:?}", sessions[0].len());
    }
}
