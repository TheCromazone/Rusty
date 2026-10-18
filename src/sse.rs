//! Reading server-sent events cheaply: one reused line buffer per stream,
//! and event fields read straight out of the line instead of through a
//! `serde_json::Value` tree.
//!
//! The field types here answer exactly what a lookup on a `Value` would:
//! `Str` is `value.as_str()`, `U64` is `value.as_u64()`, `Obj<T>` reads `T`
//! from an object and gives `T::default()` for anything else (as indexing a
//! non-object gives null), and `Field` is `value.get(key)`, where a null
//! still counts as present. So a typed event behaves as the `Value` code it
//! replaced, without building the tree.

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::marker::PhantomData;

/// Lines of a stream, read into one buffer that is reused for every line.
pub struct Lines<R> {
    reader: BufReader<R>,
    line: String,
}

impl<R: Read> Lines<R> {
    pub fn new(inner: R) -> Self {
        Self { reader: BufReader::with_capacity(64 * 1024, inner), line: String::new() }
    }

    /// The next line without its `\n` or `\r\n`, as `BufRead::lines` gives it.
    pub fn next_line(&mut self) -> Option<std::io::Result<&str>> {
        self.line.clear();
        match self.reader.read_line(&mut self.line) {
            Ok(0) => None,
            Ok(_) => {
                if self.line.ends_with('\n') {
                    self.line.pop();
                    if self.line.ends_with('\r') {
                        self.line.pop();
                    }
                }
                Some(Ok(&self.line))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

/// Reads `T` from one event's JSON, the way `Value` lookups would read it.
/// Malformed JSON gives None. A duplicated key (which typed reading rejects
/// and `Value` resolves to the last one) goes through a `Value` first.
pub fn event<'a, T: Deserialize<'a> + Default>(data: &'a str, spare: &'a mut Option<Value>) -> Option<T> {
    if let Ok(Obj(t)) = serde_json::from_str::<Obj<T>>(data) {
        return Some(t);
    }
    let v: &'a Value = spare.insert(serde_json::from_str(data).ok()?);
    Obj::<T>::deserialize(v).ok().map(|o| o.0)
}

/// Visitor methods that answer `$out` for the listed kinds of JSON value,
/// skipping over arrays and objects.
macro_rules! otherwise {
    ($out:expr; $($kind:ident)*) => { $(otherwise!(@ $kind $out);)* };
    (@ bool $out:expr) => { fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> { Ok($out) } };
    (@ i64 $out:expr) => { fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> { Ok($out) } };
    (@ u64 $out:expr) => { fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> { Ok($out) } };
    (@ f64 $out:expr) => { fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> { Ok($out) } };
    (@ str $out:expr) => { fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> { Ok($out) } };
    (@ unit $out:expr) => { fn visit_unit<E>(self) -> Result<Self::Value, E> { Ok($out) } };
    (@ seq $out:expr) => {
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok($out)
        }
    };
    (@ map $out:expr) => {
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok($out)
        }
    };
}

/// A string field: `Some` only when the JSON value is a string.
#[derive(Default)]
pub struct Str<'a>(pub Option<Cow<'a, str>>);

impl Str<'_> {
    pub fn get(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Str<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Str<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_borrowed_str<E>(self, s: &'de str) -> Result<Str<'a>, E> {
                Ok(Str(Some(Cow::Borrowed(s))))
            }
            fn visit_str<E>(self, s: &str) -> Result<Str<'a>, E> {
                Ok(Str(Some(Cow::Owned(s.to_owned()))))
            }
            fn visit_string<E>(self, s: String) -> Result<Str<'a>, E> {
                Ok(Str(Some(Cow::Owned(s))))
            }
            otherwise!(Str(None); bool i64 u64 f64 unit seq map);
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// An unsigned integer field: `Some` only for a non-negative JSON integer.
#[derive(Default, Clone, Copy)]
pub struct U64(pub Option<u64>);

impl<'de> Deserialize<'de> for U64 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = U64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_u64<E>(self, n: u64) -> Result<U64, E> {
                Ok(U64(Some(n)))
            }
            fn visit_i64<E>(self, n: i64) -> Result<U64, E> {
                Ok(U64(u64::try_from(n).ok()))
            }
            otherwise!(U64(None); bool f64 str unit seq map);
        }
        d.deserialize_any(V)
    }
}

/// A field kept as a `Value` when the key is there at all, null included.
#[derive(Default)]
pub struct Field(pub Option<Value>);

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Value::deserialize(d).map(|v| Field(Some(v)))
    }
}

/// `T` from a JSON object; `T::default()` from any other value.
#[derive(Default)]
pub struct Obj<T>(pub T);

impl<'de, T: Deserialize<'de> + Default> Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de> + Default> Visitor<'de> for V<T> {
            type Value = Obj<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Obj<T>, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(Obj)
            }
            otherwise!(Obj(T::default()); bool i64 u64 f64 str unit seq);
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// The first element of an array field (`value.get(0)`); None otherwise.
pub struct First<T>(pub Option<T>);

impl<T> Default for First<T> {
    fn default() -> Self {
        First(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for First<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = First<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<First<T>, A::Error> {
                let first = seq.next_element::<T>()?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(First(first))
            }
            otherwise!(First(None); bool i64 u64 f64 str unit map);
        }
        d.deserialize_any(V(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Probe<'a> {
        #[serde(borrow)]
        s: Str<'a>,
        n: U64,
        f: Field,
        #[serde(borrow)]
        o: Obj<Inner<'a>>,
        #[serde(borrow)]
        first: First<Obj<Inner<'a>>>,
    }

    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Inner<'a> {
        #[serde(borrow)]
        s: Str<'a>,
    }

    #[test]
    fn typed_fields_answer_as_value_lookups_do() {
        for data in [
            r#"{"s":"plain","n":3,"f":null,"o":{"s":"in"},"first":[{"s":"a"},{"s":"b"}]}"#,
            r#"{"s":"esc\né","n":-1,"f":{"x":[1,2]},"o":"not an object","first":{"s":"no"}}"#,
            r#"{"s":5,"n":1.0,"o":[{"s":"x"}],"first":["text",{"s":"b"}]}"#,
            r#"{"s":null,"n":"7","o":null,"first":[]}"#,
            r#"{"s":["a"],"n":{"k":1},"o":{"s":{"deep":true}},"first":null}"#,
            r#"{"s":"one","s":"two","n":1,"n":2}"#,
            r#"["s","n"]"#,
            r#""just a string""#,
            r#"{}"#,
        ] {
            let v: Value = serde_json::from_str(data).unwrap();
            let mut spare = None;
            let p = event::<Probe>(data, &mut spare).unwrap();
            assert_eq!(p.s.get(), v["s"].as_str(), "{data}");
            assert_eq!(p.n.0, v["n"].as_u64(), "{data}");
            assert_eq!(p.f.0.as_ref(), v.get("f"), "{data}");
            assert_eq!(p.o.0.s.get(), v["o"]["s"].as_str(), "{data}");
            let first = p.first.0.map(|o| o.0.s.get().map(String::from));
            assert_eq!(first, v["first"].get(0).map(|f| f["s"].as_str().map(String::from)), "{data}");
        }
        assert!(event::<Probe>("{not json", &mut None).is_none());
    }

    #[test]
    fn plain_strings_are_borrowed_from_the_line() {
        let data = r#"{"s":"plain","o":{"s":"esc\"aped"}}"#;
        let mut spare = None;
        let p = event::<Probe>(data, &mut spare).unwrap();
        assert!(matches!(p.s.0, Some(Cow::Borrowed("plain"))));
        assert!(matches!(p.o.0.s.0, Some(Cow::Owned(ref s)) if s == "esc\"aped"));
    }

    #[test]
    fn lines_match_bufread_lines() {
        let text = "data: a\r\n\nevent: b\ndata: é\r\nlast";
        let want: Vec<String> = BufReader::new(text.as_bytes()).lines().map(Result::unwrap).collect();
        let mut lines = Lines::new(text.as_bytes());
        let mut got = Vec::new();
        while let Some(line) = lines.next_line() {
            got.push(line.unwrap().to_string());
        }
        assert_eq!(got, want);
        let mut bad = Lines::new(&b"ok\n\xff\xfe\n"[..]);
        assert_eq!(bad.next_line().unwrap().unwrap(), "ok");
        assert!(bad.next_line().unwrap().is_err(), "invalid UTF-8 is an error, as with lines()");
    }
}
