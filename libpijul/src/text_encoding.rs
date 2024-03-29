use serde::{de::Visitor, Deserialize, Serialize};
#[cfg(feature = "text-changes")]
use std::borrow::Cow;
use std::fmt;

#[cfg(test)]
use quickcheck::{Arbitrary, Gen};

#[derive(Debug, PartialEq, Eq)]
pub struct Encoding(pub(crate) &'static encoding_rs::Encoding);

impl Encoding {
    pub(crate) fn for_label(label: &str) -> Encoding {
        Encoding(encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()).unwrap())
    }

    pub(crate) fn label(&self) -> &str {
        self.0.name()
    }

    #[cfg(feature = "text-changes")]
    pub(crate) fn decode<'a>(&self, text: &'a [u8]) -> Cow<'a, str> {
        self.0.decode(&text).0
    }

    #[cfg(feature = "text-changes")]
    pub(crate) fn encode<'a>(&self, text: &'a str) -> Cow<'a, [u8]> {
        self.0.encode(text).0
    }
}

impl Clone for Encoding {
    fn clone(&self) -> Self {
        Encoding(self.0)
    }
}

impl Serialize for Encoding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

#[cfg(test)]
// TODO: more encodings. This is not critical, since it is not used ATM.
// But the instance is needed.
impl Arbitrary for Encoding {
    fn arbitrary(g: &mut Gen) -> Self {
        g.choose(&[
            Encoding(encoding_rs::UTF_8),
            Encoding(encoding_rs::SHIFT_JIS),
        ])
        .unwrap()
        .clone()
    }
}

struct EncodingVisitor;

impl<'de> Deserialize<'de> for Encoding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        impl<'de> Visitor<'de> for EncodingVisitor {
            type Value = Encoding;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string label meeting the encoding standard https://encoding.spec.whatwg.org/#concept-encoding-get")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Encoding::for_label(v))
            }
        }

        deserializer.deserialize_str(EncodingVisitor)
    }
}
