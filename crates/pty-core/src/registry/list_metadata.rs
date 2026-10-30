//! Borrow JSON values until duplicate keys have been resolved, then deserialize
//! the effective fields directly. Retained payloads never need a `Value` tree.

use std::borrow::Cow;

use indexmap::IndexMap;
use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, forward_to_deserialize_any};
use serde_json::value::RawValue;

use super::metadata::SessionMetadata;

type Fields<'de> = IndexMap<Cow<'de, str>, &'de RawValue>;

#[cfg(test)]
thread_local! {
    pub(super) static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn parse_with_payloads(
    bytes: &[u8],
    full: bool,
) -> serde_json::Result<SessionMetadata> {
    #[cfg(test)]
    PARSES.with(|count| count.set(count.get() + 1));
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let metadata = deserializer.deserialize_map(MetadataVisitor { full })?;
    deserializer.end()?;
    Ok(metadata)
}

// Only overwritten values need a separate syntax-only walk. Effective values
// are decoded by the typed visitor, preserving number and Unicode validation.
struct JsonSyntax(u8);

fn descend<E: de::Error>(depth: u8) -> Result<u8, E> {
    if depth >= 127 {
        Err(E::custom("recursion limit exceeded"))
    } else {
        Ok(depth + 1)
    }
}

impl<'de> DeserializeSeed<'de> for JsonSyntax {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        struct SyntaxVisitor(u8);
        impl<'de> Visitor<'de> for SyntaxVisitor {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> { Ok(()) }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> { Ok(()) }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> { Ok(()) }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> { Ok(()) }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> { Ok(()) }
            fn visit_unit<E: de::Error>(self) -> Result<(), E> { Ok(()) }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                let depth = descend(self.0)?;
                while seq.next_element_seed(JsonSyntax(depth))?.is_some() {}
                Ok(())
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
                let depth = descend(self.0)?;
                while map.next_key::<DiscardString>()?.is_some() {
                    map.next_value_seed(JsonSyntax(depth))?;
                }
                Ok(())
            }
        }
        deserializer.deserialize_any(SyntaxVisitor(self.0))
    }
}

struct MetadataVisitor {
    full: bool,
}

impl<'de> Visitor<'de> for MetadataVisitor {
    type Value = SessionMetadata;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("metadata must be a JSON object")
    }

    fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        let mut fields = last_fields(map, 1)?;
        if !self.full {
            if let Some(raw) = fields.shift_remove("lastLines") {
                serde_json::from_str::<Option<DiscardStrings<false>>>(raw.get())
                    .map_err(de::Error::custom)?;
            }
            for name in ["extraEnv", "sessionEnv"] {
                if let Some(raw) = fields.shift_remove(name) {
                    Option::<DiscardStrings<true>>::deserialize(Effective(raw, 1))
                        .map_err(de::Error::custom)?;
                }
            }
        }
        SessionMetadata::deserialize(de::value::MapAccessDeserializer::new(FieldsAccess {
            fields: fields.into_iter(),
            value: None,
            depth: 1,
        })).map_err(de::Error::custom)
    }
}

// Borrow ordinary keys; escaped keys need owned decoding scratch.
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

fn last_fields<'de, M: MapAccess<'de>>(mut map: M, depth: u8) -> Result<Fields<'de>, M::Error> {
    let mut fields: Fields<'de> = IndexMap::new();
    while let Some((Key(key), value)) = map.next_entry::<Key<'de>, &'de RawValue>()? {
        if let Some(previous) = fields.insert(key, value) {
            JsonSyntax(depth)
                .deserialize(&mut serde_json::Deserializer::from_str(previous.get()))
                .map_err(de::Error::custom)?;
        }
    }
    Ok(fields)
}

struct FieldsAccess<'de> {
    fields: indexmap::map::IntoIter<Cow<'de, str>, &'de RawValue>,
    value: Option<&'de RawValue>,
    depth: u8,
}

impl<'de> MapAccess<'de> for FieldsAccess<'de> {
    type Error = serde_json::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.fields.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        match key {
            Cow::Borrowed(key) => seed.deserialize(key.into_deserializer()),
            Cow::Owned(key) => seed.deserialize(key.into_deserializer()),
        }.map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        seed.deserialize(Effective(self.value.take().expect("value follows key"), self.depth))
    }
}

// Resolve duplicates recursively, including tags, publication bindings and env
// maps. Only objects buffer key/raw-slice pairs; arrays stream their elements.
struct Effective<'de>(&'de RawValue, u8);

impl<'de> Deserializer<'de> for Effective<'de> {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0.get().as_bytes().first() {
            Some(b'{') => self.0.deserialize_map(ObjectVisitor(visitor, descend(self.1)?)),
            Some(b'[') => self.0.deserialize_seq(ArrayVisitor(visitor, descend(self.1)?)),
            _ => self.0.deserialize_any(visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.0.get() == "null" {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct enum
        identifier ignored_any
    }
}

struct ObjectVisitor<V>(V, u8);

impl<'de, V: Visitor<'de>> Visitor<'de> for ObjectVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON object")
    }
    fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        let fields = last_fields(map, self.1)?;
        self.0.visit_map(FieldsAccess { fields: fields.into_iter(), value: None, depth: self.1 })
            .map_err(de::Error::custom)
    }
}

struct ArrayVisitor<V>(V, u8);

impl<'de, V: Visitor<'de>> Visitor<'de> for ArrayVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        self.0.visit_seq(Elements(seq, self.1))
    }
}

struct Elements<A>(A, u8);

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Elements<A> {
    type Error = A::Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.0.next_element::<&'de RawValue>()?
            .map(|raw| seed.deserialize(Effective(raw, self.1)).map_err(de::Error::custom))
            .transpose()
    }
}

// Validate the effective payload's original string shapes without retaining it.
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
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if MAP {
                    return Err(de::Error::invalid_type(de::Unexpected::Seq, &self));
                }
                while seq.next_element::<DiscardString>()?.is_some() {}
                Ok(DiscardStrings)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                if !MAP {
                    return Err(de::Error::invalid_type(de::Unexpected::Map, &self));
                }
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
                "owner": "agent/example-owner", "start-operation": "agent/example-operation",
                "scope": "agent/example-scope", "strategy": "permanent"
            },
            "exitCode": 0, "exitedAt": "2026-10-21T00:00:01.000Z",
            "lastAttachAt": "2026-10-21T00:00:00.000Z", "lastOutputAtMs": 42,
            "clientGeneration": 3, "rows": 24, "cols": 80, "ephemeral": false,
            "isolateEnv": true, "unsetEnv": ["DISPLAY"], "env": {"KEEP": "value"},
            "provenance": {"owner": "agent/example-other"}, "scope": "agent/example-scope",
            "lastLines": vec!["x".repeat(100); 100],
            "extraEnv": (0..24).map(|i| (format!("KEY_{i}"), Value::String("y".repeat(90)))).collect::<serde_json::Map<_, _>>(),
            "sessionEnv": (0..24).map(|i| (format!("KEY_{i}"), Value::String("z".repeat(90)))).collect::<serde_json::Map<_, _>>()
        })).unwrap()
    }

    #[test]
    fn projection_equivalence() {
        assert_eq!(parse_with_payloads(&fixture(), true).unwrap(), old(&fixture()).unwrap());
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
    fn full_legacy_record_preserves_whole_record_identity() {
        let mut value: Value = serde_json::from_slice(&fixture()).unwrap();
        value.as_object_mut().unwrap().remove("generation");
        let bytes = serde_json::to_vec(&value).unwrap();
        let expected = old(&bytes).unwrap();
        let full = parse_with_payloads(&bytes, true).unwrap();
        assert_eq!(full, expected);
        assert!(super::super::mutate::metadata_matches_observation(&full, &expected));
        assert!(!super::super::mutate::metadata_matches_observation(
            &parse(&bytes).unwrap(), &expected,
        ));
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
            assert_eq!(parse_with_payloads(bytes, true).is_ok(), old(bytes).is_ok(), "{bytes:?}");
        }
    }

    #[test]
    fn duplicate_keys_match_old_effective_values() {
        for bytes in [
            br#"{"command":"sh","command":"bash","exitCode":0}"#.as_slice(),
            br#"{"command":42,"command":"bash"}"#,
            br#"{"command":"sh","command":42}"#,
            br#"{"command":"sh","comm\u0061nd":"bash"}"#,
            br#"{"lastLines":42,"lastLines":[]}"#,
            br#"{"lastLines":["retained"],"lastLines":null}"#,
            br#"{"lastLines":[],"lastLines":42}"#,
            br#"{"extraEnv":42,"extraEnv":{"KEY":"value"}}"#,
            br#"{"extraEnv":{},"extraEnv":null}"#,
            br#"{"extraEnv":{},"extraEnv":42}"#,
            br#"{"sessionEnv":42,"sessionEnv":{"KEY":"value"}}"#,
            br#"{"sessionEnv":{},"sessionEnv":null}"#,
            br#"{"sessionEnv":{},"sessionEnv":42}"#,
            br#"{"tags":{"owner":42,"owner":"agent/example-owner"}}"#,
            br#"{"extraEnv":{"KEY":42,"KEY":"value"}}"#,
            br#"{"sessionEnv":{"KEY":42,"KEY":"value"}}"#,
            br#"{"extraEnv":{"KEY":"value","KEY":42}}"#,
            br#"{"publicationBinding":{"operation":42,"operation":"agent/example-operation","digest":"agent/example-digest"}}"#,
            br#"{"provenance":{"owner":"agent/example-owner","owner":"agent/example-other"}}"#,
            br#"{"command":1e999,"command":"sh"}"#,
            br#"{"lastLines":["\ud800"],"lastLines":[]}"#,
            br#"{"extraEnv":{"KEY":1e999},"extraEnv":{}}"#,
            br#"{"sessionEnv":{"KEY":"\ud800"},"sessionEnv":{}}"#,
        ] {
            for full in [false, true] {
                let actual = parse_with_payloads(bytes, full);
                let expected = old(bytes).map(|mut metadata| {
                    if !full {
                        metadata.last_lines = None;
                        metadata.extra_env = None;
                        metadata.session_env = None;
                    }
                    metadata
                });
                assert_eq!(actual.is_ok(), expected.is_ok(), "{bytes:?}, full={full}");
                if let Ok(expected) = expected {
                    assert_eq!(actual.unwrap(), expected, "{bytes:?}, full={full}");
                }
            }
        }
    }

    #[test]
    fn overwritten_values_keep_the_original_json_depth_limit() {
        for nesting in [126, 127, 130] {
            for overwritten in [false, true] {
                let tail = if overwritten { ",\"provenance\":null" } else { "" };
                let bytes = format!(
                    "{{\"provenance\":{}0{}{tail}}}",
                    "[".repeat(nesting), "]".repeat(nesting),
                );
                for full in [false, true] {
                    assert_eq!(
                        parse_with_payloads(bytes.as_bytes(), full).is_ok(),
                        old(bytes.as_bytes()).is_ok(),
                        "nesting={nesting}, overwritten={overwritten}, full={full}",
                    );
                }
            }
        }
    }

    fn parse(bytes: &[u8]) -> serde_json::Result<SessionMetadata> {
        parse_with_payloads(bytes, false)
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
