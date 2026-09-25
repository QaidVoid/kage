//! Kinds of input a model accepts.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One kind of input a model accepts, as models.dev names it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    /// Plain text.
    Text,
    /// Images.
    Image,
    /// PDF documents.
    Pdf,
    /// Audio.
    Audio,
    /// Video.
    Video,
}

impl Input {
    /// Every input kind, in display order.
    pub const ALL: [Self; 5] = [Self::Text, Self::Image, Self::Pdf, Self::Audio, Self::Video];

    /// Name, such as `"image"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Pdf => "pdf",
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }

    /// Parse a name produced by [`Self::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.as_str() == s)
    }
}

/// A set of [`Input`] kinds. Empty means the inputs are unknown.
/// Serializes as a list of names.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Inputs(u8);

impl Inputs {
    /// The set holding `values`.
    #[must_use]
    pub const fn of(values: &[Input]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < values.len() {
            bits |= 1 << values[i] as u8;
            i += 1;
        }
        Self(bits)
    }

    /// Whether `input` is in the set.
    #[must_use]
    pub fn contains(self, input: Input) -> bool {
        self.0 & (1 << input as u8) != 0
    }

    /// Whether the set is empty, meaning unknown.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Whether the inputs are known and leave out `input`.
    #[must_use]
    pub fn lacks(self, input: Input) -> bool {
        !self.is_empty() && !self.contains(input)
    }

    /// The inputs in the set, in display order.
    pub fn iter(self) -> impl Iterator<Item = Input> {
        Input::ALL.into_iter().filter(move |i| self.contains(*i))
    }

    /// Names joined by spaces, such as `"text image pdf"`.
    #[must_use]
    pub fn label(self) -> String {
        self.iter().map(Input::as_str).collect::<Vec<_>>().join(" ")
    }
}

impl FromIterator<Input> for Inputs {
    fn from_iter<I: IntoIterator<Item = Input>>(iter: I) -> Self {
        Self(iter.into_iter().fold(0, |bits, i| bits | 1 << i as u8))
    }
}

impl Serialize for Inputs {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for Inputs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Vec::<Input>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_inputs_lack_nothing() {
        assert!(!Inputs::default().lacks(Input::Image));
        let text = Inputs::of(&[Input::Text]);
        assert!(text.lacks(Input::Image));
        assert!(!text.lacks(Input::Text));
    }

    #[test]
    fn inputs_roundtrip_and_label_in_order() {
        let set: Inputs = [Input::Pdf, Input::Text, Input::Image]
            .into_iter()
            .collect();
        assert_eq!(set.label(), "text image pdf");
        let v = serde_json::to_value(set).unwrap();
        assert_eq!(v, serde_json::json!(["text", "image", "pdf"]));
        assert_eq!(serde_json::from_value::<Inputs>(v).unwrap(), set);
    }
}
