//! Reads the header of a GGUF file (metadata only, never tensor data) to tell
//! embedding models from chat models and to find the pooling mode.
use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
};

/// Most header bytes read. Real headers with large tokenizer tables stay well under this.
const MAX_HEADER: u64 = 16 * 1024 * 1024;
/// Most metadata keys read. More than this is treated as a malformed file.
const MAX_KEYS: u64 = 10_000;

/// What the GGUF header says about a model file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufInfo {
    /// True for embedding models: a `<arch>.pooling_type` key, a BERT-family
    /// architecture, or a Qwen3 file whose name contains "embed".
    pub is_embedding: bool,
    /// Pooling from `<arch>.pooling_type`: none, mean, cls, last or rank.
    pub pooling: Option<String>,
}

/// Reads the metadata of a GGUF file. Returns None when the file cannot be
/// opened, is not GGUF, is truncated, or exceeds the header limits.
pub fn read_info(path: &Path) -> Option<GgufInfo> {
    let file = File::open(path).ok()?;
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    parse(BufReader::new(file), name.contains("embed"))
}

fn parse<R: Read>(reader: R, name_says_embed: bool) -> Option<GgufInfo> {
    let mut h = Header {
        inner: reader,
        left: MAX_HEADER,
    };
    if h.bytes(4)? != b"GGUF" {
        return None;
    }
    let version = h.u32()?;
    if !(2..=3).contains(&version) {
        return None;
    }
    let _tensor_count = h.u64()?;
    let kv_count = h.u64()?;
    if kv_count > MAX_KEYS {
        return None;
    }

    let mut architecture = String::new();
    // (prefix, value) for every key named "<prefix>.pooling_type".
    let mut pooling: Vec<(String, Option<u64>)> = Vec::new();
    for _ in 0..kv_count {
        let key = h.string()?;
        let ty = h.u32()?;
        let value = h.value(ty)?;
        if key == "general.architecture" {
            architecture = value.into_string().unwrap_or_default();
        } else if let Some(prefix) = key.strip_suffix(".pooling_type") {
            pooling.push((prefix.to_string(), value.uint()));
        }
    }

    let is_embedding = !pooling.is_empty()
        || architecture.contains("bert")
        || (architecture == "qwen3" && name_says_embed);
    let pooling = pooling
        .iter()
        .find(|(prefix, _)| !architecture.is_empty() && prefix == &architecture)
        .and_then(|(_, raw)| (*raw).and_then(pooling_name))
        .map(str::to_string);
    Some(GgufInfo {
        is_embedding,
        pooling,
    })
}

fn pooling_name(raw: u64) -> Option<&'static str> {
    Some(match raw {
        0 => "none",
        1 => "mean",
        2 => "cls",
        3 => "last",
        4 => "rank",
        _ => return None,
    })
}

/// Size in bytes of a fixed-size GGUF value type, None for strings, arrays and unknown types.
fn fixed_size(ty: u32) -> Option<u64> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

enum Value {
    Str(String),
    Uint(u64),
    Other,
}

impl Value {
    fn into_string(self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    fn uint(&self) -> Option<u64> {
        match self {
            Value::Uint(v) => Some(*v),
            _ => None,
        }
    }
}

/// Byte reader that refuses to consume more than the header budget.
struct Header<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Header<R> {
    fn bytes(&mut self, n: u64) -> Option<Vec<u8>> {
        if n > self.left {
            return None;
        }
        self.left -= n;
        let mut buf = vec![0; usize::try_from(n).ok()?];
        self.inner.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn skip(&mut self, n: u64) -> Option<()> {
        if n > self.left {
            return None;
        }
        self.left -= n;
        let copied = io::copy(&mut self.inner.by_ref().take(n), &mut io::sink()).ok()?;
        (copied == n).then_some(())
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N as u64)?.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u64()?;
        String::from_utf8(self.bytes(len)?).ok()
    }

    /// Reads one value of type `ty`. Arrays and other non-scalar values are skipped.
    fn value(&mut self, ty: u32) -> Option<Value> {
        match ty {
            8 => Some(Value::Str(self.string()?)),
            9 => {
                self.skip_payload(9)?;
                Some(Value::Other)
            }
            0..=5 | 10 | 11 => {
                let raw = self.bytes(fixed_size(ty)?)?;
                let uint = raw
                    .iter()
                    .rev()
                    .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                Some(Value::Uint(uint))
            }
            _ => {
                self.skip(fixed_size(ty)?)?;
                Some(Value::Other)
            }
        }
    }

    /// Skips the payload of a value of type `ty` without storing it.
    /// Nested arrays are rejected, so the walk never recurses.
    fn skip_payload(&mut self, ty: u32) -> Option<()> {
        match ty {
            8 => {
                let len = self.u64()?;
                self.skip(len)
            }
            9 => {
                let elem = self.u32()?;
                let count = self.u64()?;
                match elem {
                    8 => {
                        for _ in 0..count {
                            let len = self.u64()?;
                            self.skip(len)?;
                        }
                    }
                    9 => return None,
                    _ => {
                        let total = count.checked_mul(fixed_size(elem)?)?;
                        self.skip(total)?;
                    }
                }
                Some(())
            }
            _ => self.skip(fixed_size(ty)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn text(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u64).to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    }

    fn head(key: &str, ty: u32) -> Vec<u8> {
        let mut out = text(key);
        out.extend_from_slice(&ty.to_le_bytes());
        out
    }

    fn str_kv(key: &str, value: &str) -> Vec<u8> {
        let mut out = head(key, 8);
        out.extend(text(value));
        out
    }

    fn u32_kv(key: &str, value: u32) -> Vec<u8> {
        let mut out = head(key, 4);
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    fn array_kv(key: &str, elem: u32, payload: Vec<u8>, count: u64) -> Vec<u8> {
        let mut out = head(key, 9);
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend(payload);
        out
    }

    fn file(kvs: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"GGUF".to_vec();
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for kv in kvs {
            out.extend_from_slice(kv);
        }
        out
    }

    fn parse_bytes(data: &[u8], name_says_embed: bool) -> Option<GgufInfo> {
        parse(Cursor::new(data), name_says_embed)
    }

    fn info(is_embedding: bool, pooling: Option<&str>) -> GgufInfo {
        GgufInfo {
            is_embedding,
            pooling: pooling.map(str::to_string),
        }
    }

    #[test]
    fn chat_model_with_string_and_integer_keys() {
        let data = file(&[
            str_kv("general.architecture", "llama"),
            u32_kv("llama.context_length", 4096),
        ]);
        assert_eq!(parse_bytes(&data, false), Some(info(false, None)));
    }

    #[test]
    fn pooling_key_marks_embedding_in_any_order() {
        let data = file(&[
            u32_kv("llama.pooling_type", 1),
            str_kv("general.architecture", "llama"),
        ]);
        assert_eq!(parse_bytes(&data, false), Some(info(true, Some("mean"))));
    }

    #[test]
    fn pooling_values_map_to_names() {
        for (raw, name) in [(0, "none"), (2, "cls"), (3, "last"), (4, "rank")] {
            let data = file(&[
                str_kv("general.architecture", "bert"),
                u32_kv("bert.pooling_type", raw),
            ]);
            assert_eq!(parse_bytes(&data, false), Some(info(true, Some(name))));
        }
    }

    #[test]
    fn unknown_pooling_value_is_none_but_still_embedding() {
        let data = file(&[
            str_kv("general.architecture", "bert"),
            u32_kv("bert.pooling_type", 9),
        ]);
        assert_eq!(parse_bytes(&data, false), Some(info(true, None)));
    }

    #[test]
    fn pooling_is_read_only_for_the_declared_architecture() {
        let data = file(&[
            str_kv("general.architecture", "qwen3"),
            u32_kv("llama.pooling_type", 1),
        ]);
        assert_eq!(parse_bytes(&data, false), Some(info(true, None)));
    }

    #[test]
    fn bert_family_architectures_are_embedding() {
        for arch in ["bert", "nomic-bert", "jina-bert"] {
            let data = file(&[str_kv("general.architecture", arch)]);
            assert_eq!(parse_bytes(&data, false), Some(info(true, None)));
        }
    }

    #[test]
    fn qwen3_is_embedding_only_when_the_name_says_so() {
        let data = file(&[str_kv("general.architecture", "qwen3")]);
        assert_eq!(parse_bytes(&data, false), Some(info(false, None)));
        assert_eq!(parse_bytes(&data, true), Some(info(true, None)));
    }

    #[test]
    fn skips_arrays_and_scalar_values_before_the_pooling_key() {
        let mut strings = text("<s>");
        strings.extend(text("hello"));
        let mut floats = 1.5f32.to_le_bytes().to_vec();
        floats.extend_from_slice(&2.5f32.to_le_bytes());
        let mut flag = head("flag", 7);
        flag.push(1);
        let mut big = head("big", 10);
        big.extend_from_slice(&7u64.to_le_bytes());
        let mut half = head("f64", 12);
        half.extend_from_slice(&0.5f64.to_le_bytes());
        let mut short = head("short", 3);
        short.extend_from_slice(&(-2i16).to_le_bytes());
        let data = file(&[
            array_kv("tokenizer.ggml.tokens", 8, strings, 2),
            array_kv("tokenizer.ggml.scores", 6, floats, 2),
            array_kv("empty", 4, Vec::new(), 0),
            flag,
            big,
            half,
            short,
            str_kv("general.architecture", "llama"),
            u32_kv("llama.pooling_type", 3),
        ]);
        assert_eq!(parse_bytes(&data, false), Some(info(true, Some("last"))));
    }

    #[test]
    fn rejects_truncated_input() {
        let data = file(&[str_kv("general.architecture", "llama")]);
        assert!(parse_bytes(&data, false).is_some());
        assert_eq!(parse_bytes(&data[..data.len() - 2], false), None);
        assert_eq!(parse_bytes(&data[..6], false), None);
        assert_eq!(parse_bytes(&[], false), None);
    }

    #[test]
    fn rejects_wrong_magic_and_version() {
        let data = file(&[str_kv("general.architecture", "llama")]);
        let mut magic = data.clone();
        magic[..4].copy_from_slice(b"GGML");
        assert_eq!(parse_bytes(&magic, false), None);
        let mut v1 = data.clone();
        v1[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_bytes(&v1, false), None);
    }

    #[test]
    fn rejects_unknown_value_types_and_oversized_claims() {
        let mut unknown = head("x", 99);
        unknown.push(0);
        assert_eq!(parse_bytes(&file(&[unknown]), false), None);

        let mut too_many = b"GGUF".to_vec();
        too_many.extend_from_slice(&3u32.to_le_bytes());
        too_many.extend_from_slice(&0u64.to_le_bytes());
        too_many.extend_from_slice(&10_001u64.to_le_bytes());
        assert_eq!(parse_bytes(&too_many, false), None);

        let mut huge_key = b"GGUF".to_vec();
        huge_key.extend_from_slice(&3u32.to_le_bytes());
        huge_key.extend_from_slice(&0u64.to_le_bytes());
        huge_key.extend_from_slice(&1u64.to_le_bytes());
        huge_key.extend_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(parse_bytes(&huge_key, false), None);

        let huge_array = file(&[array_kv("x", 4, Vec::new(), 1 << 40)]);
        assert_eq!(parse_bytes(&huge_array, false), None);

        let nested = file(&[array_kv("x", 9, Vec::new(), 1)]);
        assert_eq!(parse_bytes(&nested, false), None);
    }

    #[test]
    fn missing_file_is_none() {
        assert_eq!(read_info(Path::new("no-such-dir/no-such-model.gguf")), None);
    }
}
