//! Reading YAML into Rust types.
//!
//! The events come from `granit-parser`. The way they become Rust values
//! follows serde_yaml 0.9.34, which Axocoatl used until 1.3.1, so every YAML
//! document Axocoatl read before reads to the same values now: which plain
//! scalars are null, booleans, integers and floats, that a scalar read into
//! a string is its text, how enums and tags read, the recursion and alias
//! limits, and the messages and paths of type errors.
//!
//! The deserializer below is adapted from serde_yaml 0.9.34's `src/de.rs`
//! and `src/path.rs` (<https://github.com/dtolnay/serde-yaml>, by David
//! Tolnay, licensed MIT OR Apache-2.0, used here under Apache-2.0). It was
//! changed to read granit-parser's events, to own its strings, and to return
//! errors where serde_yaml panicked on an event stream it did not expect.

use std::fmt;
use std::num::ParseIntError;
use std::sync::Arc;

use serde::de::value::StrDeserializer;
use serde::de::{
    self, Deserialize, DeserializeOwned, DeserializeSeed, Expected, IgnoredAny, Unexpected, Visitor,
};

use super::error::{self, Error, ErrorImpl, Mark};
use super::load::{Document, Event, Loader, Scalar, Style};

type Result<T, E = Error> = std::result::Result<T, E>;

/// The deepest nesting of sequences, mappings and newtypes that is read.
const RECURSION_LIMIT: u8 = 128;

pub(crate) const TAG_NULL: &str = "tag:yaml.org,2002:null";
pub(crate) const TAG_BOOL: &str = "tag:yaml.org,2002:bool";
pub(crate) const TAG_INT: &str = "tag:yaml.org,2002:int";
pub(crate) const TAG_FLOAT: &str = "tag:yaml.org,2002:float";

/// Read one YAML document into `T`.
///
/// A stream with more than one document is refused, as is a document that
/// does not parse, also after the part `T` needed.
pub fn from_str<T: DeserializeOwned>(input: &str) -> Result<T> {
    let source = super::load::lenient_source(input);
    let mut loader = Loader::new(&source);
    let document = match loader.next_document() {
        Some(document) => document,
        None => return Err(error::new(ErrorImpl::EndOfStream)),
    };
    let mut pos = 0;
    let mut jumpcount = 0;
    let value = T::deserialize(&mut DeserializerFromEvents {
        document: &document,
        pos: &mut pos,
        jumpcount: &mut jumpcount,
        path: Path::Root,
        remaining_depth: RECURSION_LIMIT,
        current_enum: None,
    })?;
    if let Some(parse_error) = document.error {
        return Err(error::shared(parse_error));
    }
    match loader.next_document() {
        None => Ok(value),
        Some(second) => {
            // Point at the second document's content, or at its syntax error.
            let mark = second
                .events
                .first()
                .map(|(_, mark)| *mark)
                .or_else(|| second.error.as_ref().and_then(|error| error.mark()));
            Err(error::new(ErrorImpl::MoreThanOneDocument(mark)))
        }
    }
}

/// Where a value is in the document, such as `agents[0].model`.
#[derive(Copy, Clone)]
pub(crate) enum Path<'a> {
    Root,
    Seq { parent: &'a Path<'a>, index: usize },
    Map { parent: &'a Path<'a>, key: &'a str },
    Alias { parent: &'a Path<'a> },
    Unknown { parent: &'a Path<'a> },
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        struct Parent<'a>(&'a Path<'a>);

        impl fmt::Display for Parent<'_> {
            fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                match self.0 {
                    Path::Root => Ok(()),
                    path => write!(formatter, "{path}."),
                }
            }
        }

        match self {
            Path::Root => formatter.write_str("."),
            Path::Seq { parent, index } => write!(formatter, "{parent}[{index}]"),
            Path::Map { parent, key } => write!(formatter, "{}{}", Parent(parent), key),
            Path::Alias { parent } => write!(formatter, "{parent}"),
            Path::Unknown { parent } => write!(formatter, "{}?", Parent(parent)),
        }
    }
}

fn unexpected_event(what: &str) -> Error {
    error::new(ErrorImpl::Message(
        format!("unexpected {what} in the YAML event stream"),
        None,
    ))
}

struct DeserializerFromEvents<'document> {
    document: &'document Document,
    pos: &'document mut usize,
    jumpcount: &'document mut usize,
    path: Path<'document>,
    remaining_depth: u8,
    current_enum: Option<CurrentEnum<'document>>,
}

#[derive(Copy, Clone)]
struct CurrentEnum<'document> {
    name: Option<&'static str>,
    tag: &'document str,
}

impl<'document> DeserializerFromEvents<'document> {
    fn peek_event(&self) -> Result<&'document Event> {
        self.peek_event_mark().map(|(event, _mark)| event)
    }

    fn peek_event_mark(&self) -> Result<(&'document Event, Mark)> {
        match self.document.events.get(*self.pos) {
            Some((event, mark)) => Ok((event, *mark)),
            None => Err(match &self.document.error {
                Some(parse_error) => error::shared(Arc::clone(parse_error)),
                None => error::new(ErrorImpl::EndOfStream),
            }),
        }
    }

    fn next_event(&mut self) -> Result<&'document Event> {
        self.next_event_mark().map(|(event, _mark)| event)
    }

    fn next_event_mark(&mut self) -> Result<(&'document Event, Mark)> {
        self.peek_event_mark().map(|(event, mark)| {
            *self.pos += 1;
            self.current_enum = None;
            (event, mark)
        })
    }

    fn jump<'anchor>(
        &'anchor mut self,
        pos: &'anchor mut usize,
    ) -> Result<DeserializerFromEvents<'anchor>> {
        *self.jumpcount += 1;
        if *self.jumpcount > self.document.events.len() * 100 {
            return Err(error::new(ErrorImpl::RepetitionLimitExceeded));
        }
        Ok(DeserializerFromEvents {
            document: self.document,
            pos,
            jumpcount: self.jumpcount,
            path: Path::Alias { parent: &self.path },
            remaining_depth: self.remaining_depth,
            current_enum: None,
        })
    }

    fn ignore_any(&mut self) -> Result<()> {
        enum Nest {
            Sequence,
            Mapping,
        }

        let mut stack = Vec::new();

        loop {
            match self.next_event()? {
                Event::Alias(_) | Event::Scalar(_) | Event::Void => {}
                Event::SequenceStart(_) => stack.push(Nest::Sequence),
                Event::MappingStart(_) => stack.push(Nest::Mapping),
                Event::SequenceEnd => match stack.pop() {
                    Some(Nest::Sequence) => {}
                    None | Some(Nest::Mapping) => return Err(unexpected_event("sequence end")),
                },
                Event::MappingEnd => match stack.pop() {
                    Some(Nest::Mapping) => {}
                    None | Some(Nest::Sequence) => return Err(unexpected_event("mapping end")),
                },
            }
            if stack.is_empty() {
                return Ok(());
            }
        }
    }

    fn visit_sequence<'de, V: Visitor<'de>>(&mut self, visitor: V, mark: Mark) -> Result<V::Value> {
        let (value, len) = self.recursion_check(mark, |de| {
            let mut seq = SeqAccess {
                empty: false,
                de,
                len: 0,
            };
            let value = visitor.visit_seq(&mut seq)?;
            Ok((value, seq.len))
        })?;
        self.end_sequence(len)?;
        Ok(value)
    }

    fn visit_mapping<'de, V: Visitor<'de>>(&mut self, visitor: V, mark: Mark) -> Result<V::Value> {
        let (value, len) = self.recursion_check(mark, |de| {
            let mut map = MapAccess {
                empty: false,
                de,
                len: 0,
                key: None,
            };
            let value = visitor.visit_map(&mut map)?;
            Ok((value, map.len))
        })?;
        self.end_mapping(len)?;
        Ok(value)
    }

    fn end_sequence(&mut self, len: usize) -> Result<()> {
        let total = {
            let mut seq = SeqAccess {
                empty: false,
                de: self,
                len,
            };
            while de::SeqAccess::next_element::<IgnoredAny>(&mut seq)?.is_some() {}
            seq.len
        };
        match self.next_event()? {
            Event::SequenceEnd | Event::Void => {}
            _ => return Err(unexpected_event("event at the end of a sequence")),
        }
        if total == len {
            Ok(())
        } else {
            struct ExpectedSeq(usize);
            impl Expected for ExpectedSeq {
                fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    if self.0 == 1 {
                        write!(formatter, "sequence of 1 element")
                    } else {
                        write!(formatter, "sequence of {} elements", self.0)
                    }
                }
            }
            Err(de::Error::invalid_length(total, &ExpectedSeq(len)))
        }
    }

    fn end_mapping(&mut self, len: usize) -> Result<()> {
        let total = {
            let mut map = MapAccess {
                empty: false,
                de: self,
                len,
                key: None,
            };
            while de::MapAccess::next_entry::<IgnoredAny, IgnoredAny>(&mut map)?.is_some() {}
            map.len
        };
        match self.next_event()? {
            Event::MappingEnd | Event::Void => {}
            _ => return Err(unexpected_event("event at the end of a mapping")),
        }
        if total == len {
            Ok(())
        } else {
            struct ExpectedMap(usize);
            impl Expected for ExpectedMap {
                fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    if self.0 == 1 {
                        write!(formatter, "map containing 1 entry")
                    } else {
                        write!(formatter, "map containing {} entries", self.0)
                    }
                }
            }
            Err(de::Error::invalid_length(total, &ExpectedMap(len)))
        }
    }

    fn recursion_check<F: FnOnce(&mut Self) -> Result<T>, T>(
        &mut self,
        mark: Mark,
        f: F,
    ) -> Result<T> {
        let previous_depth = self.remaining_depth;
        self.remaining_depth = match previous_depth.checked_sub(1) {
            Some(depth) => depth,
            None => return Err(error::new(ErrorImpl::RecursionLimitExceeded(mark))),
        };
        let result = f(self);
        self.remaining_depth = previous_depth;
        result
    }
}

struct SeqAccess<'document, 'seq> {
    empty: bool,
    de: &'seq mut DeserializerFromEvents<'document>,
    len: usize,
}

impl<'de> de::SeqAccess<'de> for SeqAccess<'_, '_> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.empty {
            return Ok(None);
        }
        match self.de.peek_event()? {
            Event::SequenceEnd | Event::Void => Ok(None),
            _ => {
                let mut element_de = DeserializerFromEvents {
                    document: self.de.document,
                    pos: self.de.pos,
                    jumpcount: self.de.jumpcount,
                    path: Path::Seq {
                        parent: &self.de.path,
                        index: self.len,
                    },
                    remaining_depth: self.de.remaining_depth,
                    current_enum: None,
                };
                self.len += 1;
                seed.deserialize(&mut element_de).map(Some)
            }
        }
    }
}

struct MapAccess<'document, 'map> {
    empty: bool,
    de: &'map mut DeserializerFromEvents<'document>,
    len: usize,
    key: Option<&'document str>,
}

impl<'de> de::MapAccess<'de> for MapAccess<'_, '_> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.empty {
            return Ok(None);
        }
        match self.de.peek_event()? {
            Event::MappingEnd | Event::Void => Ok(None),
            Event::Scalar(scalar) => {
                self.len += 1;
                self.key = Some(&scalar.value);
                seed.deserialize(&mut *self.de).map(Some)
            }
            _ => {
                self.len += 1;
                self.key = None;
                seed.deserialize(&mut *self.de).map(Some)
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let mut value_de = DeserializerFromEvents {
            document: self.de.document,
            pos: self.de.pos,
            jumpcount: self.de.jumpcount,
            path: if let Some(key) = self.key {
                Path::Map {
                    parent: &self.de.path,
                    key,
                }
            } else {
                Path::Unknown {
                    parent: &self.de.path,
                }
            },
            remaining_depth: self.de.remaining_depth,
            current_enum: None,
        };
        seed.deserialize(&mut value_de)
    }
}

struct EnumAccess<'document, 'variant> {
    de: &'variant mut DeserializerFromEvents<'document>,
    name: Option<&'static str>,
    tag: &'document str,
}

impl<'de, 'document, 'variant> de::EnumAccess<'de> for EnumAccess<'document, 'variant> {
    type Error = Error;
    type Variant = DeserializerFromEvents<'variant>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let str_de = StrDeserializer::<Error>::new(self.tag);
        let variant = seed.deserialize(str_de)?;
        let visitor = DeserializerFromEvents {
            document: self.de.document,
            pos: self.de.pos,
            jumpcount: self.de.jumpcount,
            path: self.de.path,
            remaining_depth: self.de.remaining_depth,
            current_enum: Some(CurrentEnum {
                name: self.name,
                tag: self.tag,
            }),
        };
        Ok((variant, visitor))
    }
}

impl<'de> de::VariantAccess<'de> for DeserializerFromEvents<'_> {
    type Error = Error;

    fn unit_variant(mut self) -> Result<()> {
        Deserialize::deserialize(&mut self)
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(mut self, seed: T) -> Result<T::Value> {
        seed.deserialize(&mut self)
    }

    fn tuple_variant<V: Visitor<'de>>(mut self, _len: usize, visitor: V) -> Result<V::Value> {
        de::Deserializer::deserialize_seq(&mut self, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        mut self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        de::Deserializer::deserialize_struct(&mut self, "", fields, visitor)
    }
}

struct UnitVariantAccess<'document, 'variant> {
    de: &'variant mut DeserializerFromEvents<'document>,
}

impl<'de> de::EnumAccess<'de> for UnitVariantAccess<'_, '_> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        Ok((seed.deserialize(&mut *self.de)?, self))
    }
}

impl<'de> de::VariantAccess<'de> for UnitVariantAccess<'_, '_> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _seed: T) -> Result<T::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"newtype variant",
        ))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"tuple variant",
        ))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"struct variant",
        ))
    }
}

fn visit_scalar<'de, V: Visitor<'de>>(
    visitor: V,
    scalar: &Scalar,
    tagged_already: bool,
) -> Result<V::Value> {
    let v = scalar.value.as_str();
    if let (Some(tag), false) = (&scalar.tag, tagged_already) {
        if tag == TAG_BOOL {
            return match parse_bool(v) {
                Some(v) => visitor.visit_bool(v),
                None => Err(de::Error::invalid_value(Unexpected::Str(v), &"a boolean")),
            };
        } else if tag == TAG_INT {
            return match visit_int(visitor, v) {
                Ok(result) => result,
                Err(_) => Err(de::Error::invalid_value(Unexpected::Str(v), &"an integer")),
            };
        } else if tag == TAG_FLOAT {
            return match parse_f64(v) {
                Some(v) => visitor.visit_f64(v),
                None => Err(de::Error::invalid_value(Unexpected::Str(v), &"a float")),
            };
        } else if tag == TAG_NULL {
            return match parse_null(v) {
                Some(()) => visitor.visit_unit(),
                None => Err(de::Error::invalid_value(Unexpected::Str(v), &"null")),
            };
        } else if tag.starts_with('!') && scalar.style == Style::Plain {
            return visit_untagged_scalar(visitor, v);
        }
    } else if scalar.style == Style::Plain {
        return visit_untagged_scalar(visitor, v);
    }
    visitor.visit_str(v)
}

fn parse_null(scalar: &str) -> Option<()> {
    match scalar {
        "null" | "Null" | "NULL" | "~" => Some(()),
        _ => None,
    }
}

fn parse_bool(scalar: &str) -> Option<bool> {
    match scalar {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

fn parse_unsigned_int<T>(
    scalar: &str,
    from_str_radix: fn(&str, radix: u32) -> Result<T, ParseIntError>,
) -> Option<T> {
    let unpositive = scalar.strip_prefix('+').unwrap_or(scalar);
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = unpositive.strip_prefix(prefix) {
            if rest.starts_with(['+', '-']) {
                return None;
            }
            if let Ok(int) = from_str_radix(rest, radix) {
                return Some(int);
            }
        }
    }
    if unpositive.starts_with(['+', '-']) {
        return None;
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(unpositive, 10).ok()
}

fn parse_signed_int<T>(
    scalar: &str,
    from_str_radix: fn(&str, radix: u32) -> Result<T, ParseIntError>,
) -> Option<T> {
    let unpositive = if let Some(unpositive) = scalar.strip_prefix('+') {
        if unpositive.starts_with(['+', '-']) {
            return None;
        }
        unpositive
    } else {
        scalar
    };
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = unpositive.strip_prefix(prefix) {
            if rest.starts_with(['+', '-']) {
                return None;
            }
            if let Ok(int) = from_str_radix(rest, radix) {
                return Some(int);
            }
        }
        if let Some(rest) = scalar
            .strip_prefix('-')
            .and_then(|s| s.strip_prefix(prefix))
        {
            let negative = format!("-{rest}");
            if let Ok(int) = from_str_radix(&negative, radix) {
                return Some(int);
            }
        }
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(unpositive, 10).ok()
}

fn parse_negative_int<T>(
    scalar: &str,
    from_str_radix: fn(&str, radix: u32) -> Result<T, ParseIntError>,
) -> Option<T> {
    for (prefix, radix) in [("-0x", 16), ("-0o", 8), ("-0b", 2)] {
        if let Some(rest) = scalar.strip_prefix(prefix) {
            let negative = format!("-{rest}");
            if let Ok(int) = from_str_radix(&negative, radix) {
                return Some(int);
            }
        }
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(scalar, 10).ok()
}

pub(crate) fn parse_f64(scalar: &str) -> Option<f64> {
    let unpositive = if let Some(unpositive) = scalar.strip_prefix('+') {
        if unpositive.starts_with(['+', '-']) {
            return None;
        }
        unpositive
    } else {
        scalar
    };
    if let ".inf" | ".Inf" | ".INF" = unpositive {
        return Some(f64::INFINITY);
    }
    if let "-.inf" | "-.Inf" | "-.INF" = scalar {
        return Some(f64::NEG_INFINITY);
    }
    if let ".nan" | ".NaN" | ".NAN" = scalar {
        return Some(f64::NAN.copysign(1.0));
    }
    if let Ok(float) = unpositive.parse::<f64>() {
        if float.is_finite() {
            return Some(float);
        }
    }
    None
}

/// Leading zeros followed by digits are a string, not an octal or decimal
/// number.
pub(crate) fn digits_but_not_number(scalar: &str) -> bool {
    let scalar = scalar.strip_prefix(['-', '+']).unwrap_or(scalar);
    scalar.len() > 1 && scalar.starts_with('0') && scalar[1..].bytes().all(|b| b.is_ascii_digit())
}

fn visit_int<'de, V: Visitor<'de>>(visitor: V, v: &str) -> Result<Result<V::Value>, V> {
    if let Some(int) = parse_unsigned_int(v, u64::from_str_radix) {
        return Ok(visitor.visit_u64(int));
    }
    if let Some(int) = parse_negative_int(v, i64::from_str_radix) {
        return Ok(visitor.visit_i64(int));
    }
    if let Some(int) = parse_unsigned_int(v, u128::from_str_radix) {
        return Ok(visitor.visit_u128(int));
    }
    if let Some(int) = parse_negative_int(v, i128::from_str_radix) {
        return Ok(visitor.visit_i128(int));
    }
    Err(visitor)
}

/// How a plain scalar without a core tag reads when the target takes any
/// type: null, a boolean, an integer, a float, or else its text.
pub(crate) fn visit_untagged_scalar<'de, V: Visitor<'de>>(visitor: V, v: &str) -> Result<V::Value> {
    if v.is_empty() || parse_null(v) == Some(()) {
        return visitor.visit_unit();
    }
    if let Some(boolean) = parse_bool(v) {
        return visitor.visit_bool(boolean);
    }
    let visitor = match visit_int(visitor, v) {
        Ok(result) => return result,
        Err(visitor) => visitor,
    };
    if !digits_but_not_number(v) {
        if let Some(float) = parse_f64(v) {
            return visitor.visit_f64(float);
        }
    }
    visitor.visit_str(v)
}

fn is_plain_or_tagged_literal_scalar(
    expected: &str,
    scalar: &Scalar,
    tagged_already: bool,
) -> bool {
    match (scalar.style, &scalar.tag, tagged_already) {
        (Style::Plain, _, _) => true,
        (Style::Literal, Some(tag), false) => tag == expected,
        _ => false,
    }
}

fn invalid_type(event: &Event, exp: &dyn Expected) -> Error {
    enum Void {}

    struct InvalidType<'a> {
        exp: &'a dyn Expected,
    }

    impl<'de> Visitor<'de> for InvalidType<'_> {
        type Value = Void;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            self.exp.fmt(formatter)
        }
    }

    match event {
        Event::Alias(_) => unexpected_event("alias"),
        Event::Scalar(scalar) => {
            let get_type = InvalidType { exp };
            match visit_scalar(get_type, scalar, false) {
                Ok(void) => match void {},
                Err(invalid_type) => invalid_type,
            }
        }
        Event::SequenceStart(_) => de::Error::invalid_type(Unexpected::Seq, exp),
        Event::MappingStart(_) => de::Error::invalid_type(Unexpected::Map, exp),
        Event::SequenceEnd => unexpected_event("sequence end"),
        Event::MappingEnd => unexpected_event("mapping end"),
        Event::Void => error::new(ErrorImpl::EndOfStream),
    }
}

/// A local tag without its `!`, which names an enum variant.
fn parse_tag(tag: &Option<String>) -> Option<&str> {
    let tag = tag.as_deref()?;
    let rest = tag.strip_prefix('!')?;
    Some(if rest.is_empty() { tag } else { rest })
}

macro_rules! deserialize_number {
    ($method:ident, $visit:ident, $parse:ident, $ty:ty, $tag:expr) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            let tagged_already = self.current_enum.is_some();
            let (next, mark) = self.next_event_mark()?;
            'value: {
                match next {
                    Event::Alias(pos) => {
                        let mut pos = *pos;
                        break 'value self.jump(&mut pos)?.$method(visitor);
                    }
                    Event::Scalar(scalar)
                        if is_plain_or_tagged_literal_scalar($tag, scalar, tagged_already) =>
                    {
                        if let Some(int) = $parse(&scalar.value, <$ty>::from_str_radix) {
                            break 'value visitor.$visit(int);
                        }
                    }
                    _ => {}
                }
                break 'value Err(invalid_type(next, &visitor));
            }
            .map_err(|err| error::fix_mark(err, mark, self.path))
        }
    };
}

impl<'de> de::Deserializer<'de> for &mut DeserializerFromEvents<'_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        fn enum_tag(tag: &Option<String>, tagged_already: bool) -> Option<&str> {
            if tagged_already {
                return None;
            }
            parse_tag(tag)
        }
        match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_any(visitor)
            }
            Event::Scalar(scalar) => {
                if let Some(tag) = enum_tag(&scalar.tag, tagged_already) {
                    *self.pos -= 1;
                    visitor.visit_enum(EnumAccess {
                        de: self,
                        name: None,
                        tag,
                    })
                } else {
                    visit_scalar(visitor, scalar, tagged_already)
                }
            }
            Event::SequenceStart(tag) => {
                if let Some(tag) = enum_tag(tag, tagged_already) {
                    *self.pos -= 1;
                    visitor.visit_enum(EnumAccess {
                        de: self,
                        name: None,
                        tag,
                    })
                } else {
                    self.visit_sequence(visitor, mark)
                }
            }
            Event::MappingStart(tag) => {
                if let Some(tag) = enum_tag(tag, tagged_already) {
                    *self.pos -= 1;
                    visitor.visit_enum(EnumAccess {
                        de: self,
                        name: None,
                        tag,
                    })
                } else {
                    self.visit_mapping(visitor, mark)
                }
            }
            Event::SequenceEnd => Err(unexpected_event("sequence end")),
            Event::MappingEnd => Err(unexpected_event("mapping end")),
            Event::Void => visitor.visit_none(),
        }
        // A message made by a `Deserialize` impl has no position yet; give it
        // the position of this value.
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        'value: {
            match next {
                Event::Alias(pos) => {
                    let mut pos = *pos;
                    break 'value self.jump(&mut pos)?.deserialize_bool(visitor);
                }
                Event::Scalar(scalar)
                    if is_plain_or_tagged_literal_scalar(TAG_BOOL, scalar, tagged_already) =>
                {
                    if let Some(boolean) = parse_bool(&scalar.value) {
                        break 'value visitor.visit_bool(boolean);
                    }
                }
                _ => {}
            }
            break 'value Err(invalid_type(next, &visitor));
        }
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_i64(visitor)
    }

    deserialize_number!(deserialize_i64, visit_i64, parse_signed_int, i64, TAG_INT);
    deserialize_number!(
        deserialize_i128,
        visit_i128,
        parse_signed_int,
        i128,
        TAG_INT
    );

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_u64(visitor)
    }

    deserialize_number!(deserialize_u64, visit_u64, parse_unsigned_int, u64, TAG_INT);
    deserialize_number!(
        deserialize_u128,
        visit_u128,
        parse_unsigned_int,
        u128,
        TAG_INT
    );

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        'value: {
            match next {
                Event::Alias(pos) => {
                    let mut pos = *pos;
                    break 'value self.jump(&mut pos)?.deserialize_f64(visitor);
                }
                Event::Scalar(scalar)
                    if is_plain_or_tagged_literal_scalar(TAG_FLOAT, scalar, tagged_already) =>
                {
                    if let Some(float) = parse_f64(&scalar.value) {
                        break 'value visitor.visit_f64(float);
                    }
                }
                _ => {}
            }
            break 'value Err(invalid_type(next, &visitor));
        }
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (next, mark) = self.next_event_mark()?;
        match next {
            Event::Scalar(scalar) => visitor.visit_str(&scalar.value),
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_str(visitor)
            }
            other => Err(invalid_type(other, &visitor)),
        }
        .map_err(|err: Error| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error::new(ErrorImpl::BytesUnsupported))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error::new(ErrorImpl::BytesUnsupported))
    }

    /// `null`, `~`, an empty value or nothing at all is `None`.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let is_some = match self.peek_event()? {
            Event::Alias(pos) => {
                let mut pos = *pos;
                *self.pos += 1;
                return self.jump(&mut pos)?.deserialize_option(visitor);
            }
            Event::Scalar(scalar) => {
                let tagged_already = self.current_enum.is_some();
                if scalar.style != Style::Plain {
                    true
                } else if let (Some(tag), false) = (&scalar.tag, tagged_already) {
                    if tag == TAG_NULL {
                        if let Some(()) = parse_null(&scalar.value) {
                            false
                        } else {
                            return Err(de::Error::invalid_value(
                                Unexpected::Str(&scalar.value),
                                &"null",
                            ));
                        }
                    } else {
                        true
                    }
                } else {
                    !scalar.value.is_empty() && parse_null(&scalar.value).is_none()
                }
            }
            Event::SequenceStart(_) | Event::MappingStart(_) => true,
            Event::SequenceEnd => return Err(unexpected_event("sequence end")),
            Event::MappingEnd => return Err(unexpected_event("mapping end")),
            Event::Void => false,
        };
        if is_some {
            visitor.visit_some(self)
        } else {
            *self.pos += 1;
            self.current_enum = None;
            visitor.visit_none()
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        match next {
            Event::Scalar(scalar) => {
                let is_null = if scalar.style != Style::Plain {
                    false
                } else if let (Some(tag), false) = (&scalar.tag, tagged_already) {
                    tag == TAG_NULL && parse_null(&scalar.value).is_some()
                } else {
                    scalar.value.is_empty() || parse_null(&scalar.value).is_some()
                };
                if is_null {
                    visitor.visit_unit()
                } else {
                    Err(de::Error::invalid_value(
                        Unexpected::Str(&scalar.value),
                        &"null",
                    ))
                }
            }
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_unit(visitor)
            }
            Event::Void => visitor.visit_unit(),
            other => Err(invalid_type(other, &visitor)),
        }
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(visitor)
    }

    /// A newtype struct reads as the value it wraps.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        let (_event, mark) = self.peek_event_mark()?;
        self.recursion_check(mark, |de| visitor.visit_newtype_struct(de))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (next, mark) = self.next_event_mark()?;
        match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_seq(visitor)
            }
            Event::SequenceStart(_) => self.visit_sequence(visitor, mark),
            other => {
                if match other {
                    Event::Void => true,
                    Event::Scalar(scalar) => {
                        scalar.value.is_empty() && scalar.style == Style::Plain
                    }
                    _ => false,
                } {
                    visitor.visit_seq(SeqAccess {
                        empty: true,
                        de: self,
                        len: 0,
                    })
                } else {
                    Err(invalid_type(other, &visitor))
                }
            }
        }
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (next, mark) = self.next_event_mark()?;
        match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_map(visitor)
            }
            Event::MappingStart(_) => self.visit_mapping(visitor, mark),
            other => {
                if match other {
                    Event::Void => true,
                    Event::Scalar(scalar) => {
                        scalar.value.is_empty() && scalar.style == Style::Plain
                    }
                    _ => false,
                } {
                    visitor.visit_map(MapAccess {
                        empty: true,
                        de: self,
                        len: 0,
                        key: None,
                    })
                } else {
                    Err(invalid_type(other, &visitor))
                }
            }
        }
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    /// A unit variant is its name; a variant with data is the data tagged
    /// with the name, such as `!Variant value`.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let (next, mark) = self.peek_event_mark()?;
        'value: {
            if let Some(current_enum) = self.current_enum {
                if let Event::Scalar(scalar) = next {
                    if !scalar.value.is_empty() {
                        break 'value visitor.visit_enum(UnitVariantAccess { de: self });
                    }
                }
                let message = if let Some(name) = current_enum.name {
                    format!(
                        "deserializing nested enum in {}::{} from YAML is not supported yet",
                        name, current_enum.tag,
                    )
                } else {
                    format!(
                        "deserializing nested enum in !{} from YAML is not supported yet",
                        current_enum.tag,
                    )
                };
                break 'value Err(error::new(ErrorImpl::Message(message, None)));
            }
            break 'value match next {
                Event::Alias(pos) => {
                    let mut pos = *pos;
                    *self.pos += 1;
                    self.jump(&mut pos)?
                        .deserialize_enum(name, variants, visitor)
                }
                Event::Scalar(scalar) => {
                    if let Some(tag) = parse_tag(&scalar.tag) {
                        return visitor.visit_enum(EnumAccess {
                            de: self,
                            name: Some(name),
                            tag,
                        });
                    }
                    visitor.visit_enum(UnitVariantAccess { de: self })
                }
                Event::MappingStart(tag) => {
                    if let Some(tag) = parse_tag(tag) {
                        return visitor.visit_enum(EnumAccess {
                            de: self,
                            name: Some(name),
                            tag,
                        });
                    }
                    let err =
                        de::Error::invalid_type(Unexpected::Map, &"a YAML tag starting with '!'");
                    Err(error::fix_mark(err, mark, self.path))
                }
                Event::SequenceStart(tag) => {
                    if let Some(tag) = parse_tag(tag) {
                        return visitor.visit_enum(EnumAccess {
                            de: self,
                            name: Some(name),
                            tag,
                        });
                    }
                    let err =
                        de::Error::invalid_type(Unexpected::Seq, &"a YAML tag starting with '!'");
                    Err(error::fix_mark(err, mark, self.path))
                }
                Event::SequenceEnd => Err(unexpected_event("sequence end")),
                Event::MappingEnd => Err(unexpected_event("mapping end")),
                Event::Void => Err(error::new(ErrorImpl::EndOfStream)),
            };
        }
        .map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.ignore_any()?;
        visitor.visit_unit()
    }
}
