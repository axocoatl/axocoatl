//! A YAML document as a tree, for looking through a document whose shape is
//! not known in advance.
//!
//! It reads as serde_yaml 0.9.34's `Value` read (adapted from its
//! `src/value/de.rs` and `src/mapping.rs`, by David Tolnay, MIT OR
//! Apache-2.0, used here under Apache-2.0), so the checks that walk a
//! configuration this way see what they saw before.

use std::fmt;

use serde::de::{
    self, Deserialize, Deserializer, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor,
};

/// Any YAML value. A mapping keeps its entries in document order and, like
/// serde_yaml's, refuses a key it already holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Sequence(Vec<Value>),
    Mapping(Vec<(Value, Value)>),
    /// A value with a local tag such as `!Variant`; the tag keeps its `!`.
    Tagged(String, Box<Value>),
}

/// A YAML number, as serde_yaml kept it: a positive integer, a negative
/// one, or a float.
#[derive(Clone, Copy, Debug)]
pub enum Number {
    PosInt(u64),
    NegInt(i64),
    Float(f64),
}

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        match (self, other) {
            (Number::PosInt(a), Number::PosInt(b)) => a == b,
            (Number::NegInt(a), Number::NegInt(b)) => a == b,
            (Number::Float(a), Number::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            _ => false,
        }
    }
}

impl fmt::Display for Number {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Number::PosInt(n) => write!(formatter, "{n}"),
            Number::NegInt(n) => write!(formatter, "{n}"),
            Number::Float(f) if f.is_nan() => formatter.write_str(".nan"),
            Number::Float(f) if f.is_infinite() => formatter.write_str(if f.is_sign_negative() {
                "-.inf"
            } else {
                ".inf"
            }),
            Number::Float(f) => write!(formatter, "{f:?}"),
        }
    }
}

impl Value {
    /// The value of `key` in a mapping, looking through a tag.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Mapping(entries) => entries
                .iter()
                .find(|(entry_key, _)| matches!(entry_key, Value::String(text) if text == key))
                .map(|(_, value)| value),
            Value::Tagged(_, value) => value.get(key),
            _ => None,
        }
    }
}

fn duplicate_key(key: &Value) -> String {
    let mut message = String::from("duplicate entry ");
    match key {
        Value::Null => message.push_str("with null key"),
        Value::Bool(boolean) => message.push_str(&format!("with key `{boolean}`")),
        Value::Number(number) => message.push_str(&format!("with key {number}")),
        Value::String(string) => message.push_str(&format!("with key {string:?}")),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(..) => {
            message.push_str("in YAML map")
        }
    }
    message
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("any YAML value")
            }

            fn visit_bool<E: de::Error>(self, b: bool) -> Result<Value, E> {
                Ok(Value::Bool(b))
            }

            fn visit_i64<E: de::Error>(self, i: i64) -> Result<Value, E> {
                Ok(Value::Number(if i < 0 {
                    Number::NegInt(i)
                } else {
                    Number::PosInt(i as u64)
                }))
            }

            fn visit_u64<E: de::Error>(self, u: u64) -> Result<Value, E> {
                Ok(Value::Number(Number::PosInt(u)))
            }

            fn visit_f64<E: de::Error>(self, f: f64) -> Result<Value, E> {
                Ok(Value::Number(Number::Float(f)))
            }

            fn visit_str<E: de::Error>(self, s: &str) -> Result<Value, E> {
                Ok(Value::String(s.to_owned()))
            }

            fn visit_string<E: de::Error>(self, s: String) -> Result<Value, E> {
                Ok(Value::String(s))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            fn visit_none<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
                Deserialize::deserialize(deserializer)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Value::Sequence(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                let mut entries: Vec<(Value, Value)> = Vec::new();
                while let Some(key) = map.next_key::<Value>()? {
                    if entries.iter().any(|(existing, _)| *existing == key) {
                        return Err(de::Error::custom(duplicate_key(&key)));
                    }
                    let value = map.next_value()?;
                    entries.push((key, value));
                }
                Ok(Value::Mapping(entries))
            }

            fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
                let (tag, contents) = data.variant_seed(TagVisitor)?;
                let value = contents.newtype_variant()?;
                Ok(Value::Tagged(tag, Box::new(value)))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

/// A tag as the name of an enum variant; it gets its `!` back.
struct TagVisitor;

impl<'de> de::DeserializeSeed<'de> for TagVisitor {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_string(self)
    }
}

impl Visitor<'_> for TagVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a YAML tag string")
    }

    fn visit_str<E: de::Error>(self, string: &str) -> Result<String, E> {
        if string.is_empty() {
            return Err(E::custom("empty YAML tag is not allowed"));
        }
        Ok(if string.starts_with('!') {
            string.to_owned()
        } else {
            format!("!{string}")
        })
    }
}
