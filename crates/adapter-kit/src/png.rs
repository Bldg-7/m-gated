//! 요청 본문에 든 base64 PNG 이미지 검사(결정 0013). Anthropic과 OpenAI가 같이 쓴다.
//! 64 KiB를 읽을 때마다 [`Cooperate::cooperate`]를 기다린다. 본체는 이때 tokio에 실행을 넘기고,
//! WASM 어댑터는 넘기지 않는다.
use crate::{
    encoding::base64_decode_block,
    json::{Cooperate, JsonError},
};

/// 이미지 하나를 푼 크기의 상한.
pub const PNG_BYTES: usize = 4 * 1024 * 1024;
/// 가로와 세로 크기의 상한.
pub const PNG_SIDE: u32 = 8192;

/// JSON 문자열의 내용(따옴표 안쪽)으로 받은 base64 PNG를 검사하고, 푼 크기를 돌려준다.
///
/// - JSON escape는 `\/`와 0–255의 `\u`만 받는다.
/// - 푼 크기는 [`PNG_BYTES`] 이하다. `=` 채움 뒤에는 아무것도 올 수 없다.
/// - PNG 서명과 `IHDR`이 있어야 하고, 가로와 세로는 1–[`PNG_SIDE`]다.
pub async fn validate_png_input_with<C: Cooperate>(
    input: &[u8],
    cooperate: &mut C,
) -> Result<usize, JsonError> {
    let mut i = 0;
    let mut block = [0u8; 4];
    let mut used = 0;
    let mut header = Vec::with_capacity(33);
    let mut count = 0;
    let mut ended = false;
    let mut yielded = 0;
    while i < input.len() {
        if i - yielded >= 64 * 1024 {
            yielded = i;
            cooperate.cooperate().await;
        }
        if ended {
            return Err(JsonError);
        }
        let mut b = input[i];
        i += 1;
        if b == b'\\' {
            b = *input.get(i).ok_or(JsonError)?;
            i += 1;
            if b == b'u' {
                let digits = input.get(i..i + 4).ok_or(JsonError)?;
                let n =
                    u16::from_str_radix(std::str::from_utf8(digits).map_err(|_| JsonError)?, 16)
                        .map_err(|_| JsonError)?;
                b = u8::try_from(n).map_err(|_| JsonError)?;
                i += 4;
            } else if b != b'/' {
                return Err(JsonError);
            }
        }
        block[used] = b;
        used += 1;
        if used == 4 {
            let mut decoded = [0u8; 3];
            let n = base64_decode_block(block, &mut decoded).ok_or(JsonError)?;
            count += n;
            if count > PNG_BYTES {
                return Err(JsonError);
            }
            for b in decoded.into_iter().take(n) {
                if header.len() < 33 {
                    header.push(b);
                }
            }
            ended = block.contains(&b'=');
            used = 0;
        }
    }
    if used != 0
        || header.len() < 33
        || &header[..8] != b"\x89PNG\r\n\x1a\n"
        || &header[12..16] != b"IHDR"
    {
        return Err(JsonError);
    }
    let w = u32::from_be_bytes(header[16..20].try_into().map_err(|_| JsonError)?);
    let h = u32::from_be_bytes(header[20..24].try_into().map_err(|_| JsonError)?);
    if w == 0 || h == 0 || w > PNG_SIDE || h > PNG_SIDE {
        return Err(JsonError);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{NoYield, ready};
    use base64::Engine;
    fn png(w: u32, h: u32, extra: usize) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b.resize(b.len() + extra, 0);
        b
    }
    fn check(raw: &[u8]) -> Result<usize, JsonError> {
        ready(validate_png_input_with(raw, &mut NoYield))
    }
    #[test]
    fn size_signature_and_escapes() {
        // 34바이트는 `==` 채움으로 끝난다.
        let ok = base64::engine::general_purpose::STANDARD.encode(png(1, 1, 1));
        assert!(ok.ends_with("=="));
        assert_eq!(check(ok.as_bytes()), Ok(34));
        assert_eq!(check(ok.replace('/', "\\/").as_bytes()), Ok(34));
        assert_eq!(check(ok.replace('A', "\\u0041").as_bytes()), Ok(34));
        for bad in [
            base64::engine::general_purpose::STANDARD.encode(png(0, 1, 0)),
            base64::engine::general_purpose::STANDARD.encode(png(8193, 1, 0)),
            base64::engine::general_purpose::STANDARD.encode(b"not-png"),
            base64::engine::general_purpose::STANDARD.encode(png(1, 1, PNG_BYTES)),
            format!("{ok}AAAA"),
            format!("{}\\n", &ok[..ok.len() - 1]),
            ok[..ok.len() - 1].to_string(),
        ] {
            assert!(
                check(bad.as_bytes()).is_err(),
                "{}",
                &bad[..bad.len().min(40)]
            );
        }
    }
    #[test]
    fn long_input_cooperates_every_64_kib() {
        struct Count(usize);
        impl Cooperate for Count {
            fn cooperate(&mut self) -> impl std::future::Future<Output = ()> + Send {
                self.0 += 1;
                std::future::ready(())
            }
        }
        let raw = base64::engine::general_purpose::STANDARD.encode(png(1, 1, 300 * 1024));
        let mut count = Count(0);
        assert!(ready(validate_png_input_with(raw.as_bytes(), &mut count)).is_ok());
        assert_eq!(count.0, raw.len().div_ceil(64 * 1024) - 1);
    }
}
