//! (#3210) Prompt-processing (prefill) progress, as an engine reports it in
//! its response stream, normalized into one neutral fact.
//!
//! Recognized by the stream's SHAPE, never by configuration: an engine that
//! sends nothing of the kind yields nothing, and the reading stays what it
//! was (PROMPT with elapsed time). Both stream readers parse through this
//! one function, the runtime's (`runtime/src/lmstudio.rs`, `ChunkStream`)
//! and the host's direct path (`darkmux-crew`'s `direct_stream::SseTail`),
//! so the two cannot recognize different shapes.
//!
//! Shapes recognized today:
//!
//! - **`mlx_lm.server`** (verified on 0.31.3 and in 0.32.1's source): before
//!   the first `data:` event it writes one SSE comment line per prefill
//!   step, `: keepalive <processed>/<total>`, in prompt tokens. Nothing has
//!   to be asked for.
//!
//! Every other comment line (OpenRouter's `: OPENROUTER PROCESSING`, a bare
//! `: keepalive`) is not progress and reads as nothing, as before.

/// One progress report: `processed` of `total` prompt tokens read so far.
/// `processed <= total` and `total > 0`, or the report is not recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptProgressSample {
    pub processed: u64,
    pub total: u64,
}

impl PromptProgressSample {
    /// The progress one SSE line reports, or `None` when it reports none.
    /// `line` is one line of the stream, its line ending optional.
    pub fn of_sse_line(line: &str) -> Option<Self> {
        let comment = line.trim_end_matches(['\r', '\n']).strip_prefix(':')?;
        let rest = comment.trim_start().strip_prefix("keepalive")?;
        // `keepalive` is a whole word: `keepalives 1/2` is not this shape.
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let (processed, total) = rest.trim().split_once('/')?;
        let digits = |s: &str| {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u64>().ok())
                .flatten()
        };
        let (processed, total) = (digits(processed)?, digits(total)?);
        (total > 0 && processed <= total).then_some(Self { processed, total })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The issue's capture (#3210): `mlx_lm.server` 0.31.3, Qwen3-1.7B-4bit,
    /// `--prefill-step-size 512`, a 2,516-token prompt, as the server writes
    /// each line (`: keepalive {processed}/{total}\n\n`, its source).
    const MLX_TRANSCRIPT: &str = ": keepalive 512/2516\n\n\
: keepalive 1024/2516\n\n\
: keepalive 1536/2516\n\n\
: keepalive 2048/2516\n\n\
: keepalive 2512/2516\n\n\
: keepalive 2516/2516\n\n\
data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n";

    #[test]
    fn every_mlx_keepalive_line_of_the_capture_reads_as_its_progress() {
        let read: Vec<(u64, u64)> = MLX_TRANSCRIPT
            .lines()
            .filter_map(PromptProgressSample::of_sse_line)
            .map(|p| (p.processed, p.total))
            .collect();
        assert_eq!(
            read,
            vec![
                (512, 2516),
                (1024, 2516),
                (1536, 2516),
                (2048, 2516),
                (2512, 2516),
                (2516, 2516)
            ]
        );
    }

    #[test]
    fn a_line_ending_and_padding_do_not_hide_the_progress() {
        for line in [
            ": keepalive 512/2516\r\n",
            ": keepalive  512/2516",
            ":keepalive 512/2516\n",
        ] {
            assert_eq!(
                PromptProgressSample::of_sse_line(line),
                Some(PromptProgressSample {
                    processed: 512,
                    total: 2516
                }),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_line_that_is_not_progress_reads_as_nothing() {
        for line in [
            "",
            ": keepalive",
            ": OPENROUTER PROCESSING",
            ": keepalive 512",
            ": keepalive 512/",
            ": keepalive /2516",
            ": keepalive 512/0",
            ": keepalive 2600/2516",
            ": keepalive -1/2516",
            ": keepalive 1.5/2516",
            ": keepalive 512/2516 extra",
            ": keepalives 512/2516",
            ": keepalive512/2516",
            "data: {\"processed\":512}",
            "keepalive 512/2516",
            "event: keepalive 512/2516",
        ] {
            assert_eq!(PromptProgressSample::of_sse_line(line), None, "{line:?}");
        }
    }
}
