use super::{ContextError, ModelObservation, MAX_OBSERVATION_BYTES};
use ring::digest;

#[cfg(test)]
use crate::agent_loop::WorkspaceObservation;

// Bound the temporary model-facing copy before it can duplicate an arbitrarily long task turn.
const MAX_COMPACTION_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const CHARACTER_INDEX_STRIDE: usize = 256;

/// Sparse UTF-8 character index so a large observation does not allocate one `usize` per char.
struct CharacterBoundaries {
    checkpoints: Vec<usize>,
    character_count: usize,
    byte_len: usize,
}

impl CharacterBoundaries {
    fn new(text: &str) -> Self {
        let mut checkpoints = Vec::new();
        let mut character_count = 0_usize;
        for (byte_index, _) in text.char_indices() {
            if character_count.is_multiple_of(CHARACTER_INDEX_STRIDE) {
                checkpoints.push(byte_index);
            }
            character_count += 1;
        }
        Self {
            checkpoints,
            character_count,
            byte_len: text.len(),
        }
    }

    fn byte_at_character(&self, text: &str, character_index: usize) -> usize {
        if character_index >= self.character_count {
            return self.byte_len;
        }
        let checkpoint_index = character_index / CHARACTER_INDEX_STRIDE;
        let checkpoint_character = checkpoint_index * CHARACTER_INDEX_STRIDE;
        let checkpoint_byte = self.checkpoints[checkpoint_index];
        let relative_character = character_index - checkpoint_character;
        checkpoint_byte
            + text[checkpoint_byte..]
                .char_indices()
                .nth(relative_character)
                .map_or(0, |(byte_index, _)| byte_index)
    }
}

/// Per-request accounting for an opt-in model-facing observation view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct ObservationCompactionReport {
    pub(crate) original_bytes: usize,
    pub(crate) sent_bytes: usize,
    pub(crate) compacted_fields: usize,
    pub(crate) omitted_bytes: usize,
}

/// A compacted, model-facing copy. The authoritative Harness observations are never changed.
pub(crate) struct CompactedObservations<T> {
    pub(crate) observations: Vec<T>,
    pub(crate) report: ObservationCompactionReport,
}

/// Compact older observation text first, preserving structured event metadata and prioritizing
/// newer evidence. Changed fields carry their original byte count and SHA-256 digest. Non-text
/// structure is never dropped; if it cannot fit, the caller receives the existing context error.
pub(crate) fn compact_observations_for_model<T: ModelObservation>(
    observations: &[T],
    budget_bytes: usize,
) -> Result<Option<CompactedObservations<T>>, ContextError> {
    if budget_bytes == 0 || budget_bytes > MAX_OBSERVATION_BYTES {
        return Err(ContextError::ObservationsTooLarge);
    }

    let mut current_estimated_bytes = estimated_observation_bytes(observations);
    if current_estimated_bytes > MAX_COMPACTION_SOURCE_BYTES {
        return Err(ContextError::ObservationsTooLarge);
    }
    let original_bytes = serialized_observation_bytes(observations)?;
    if original_bytes > MAX_COMPACTION_SOURCE_BYTES {
        return Err(ContextError::ObservationsTooLarge);
    }
    if original_bytes <= budget_bytes && current_estimated_bytes <= MAX_OBSERVATION_BYTES {
        return Ok(None);
    }

    let mut compacted = observations.to_vec();
    let mut current_bytes = original_bytes;
    let mut compacted_fields = 0_usize;
    let mut omitted_bytes = 0_usize;
    for observation in &mut compacted {
        let mut observation_bytes = serialized_observation_size(observation)?;
        let mut observation_estimated_bytes = observation.estimated_size();
        for text_index in 0..observation.compactable_text_count() {
            if fits(current_bytes, current_estimated_bytes, budget_bytes) {
                break;
            }
            let Some(original) = observation.compactable_text_at(text_index).cloned() else {
                continue;
            };
            if original.is_empty() {
                continue;
            }

            let character_boundaries = CharacterBoundaries::new(&original);
            let character_count = character_boundaries.character_count;
            let digest = sha256_hex(original.as_bytes());
            let minimum = compacted_text(&original, &character_boundaries, 0, &digest);
            if minimum.len() >= original.len() {
                continue;
            }
            set_compactable_text(observation, text_index, minimum);
            let minimum_observation_bytes = serialized_observation_size(observation)?;
            let minimum_observation_estimated_bytes = observation.estimated_size();
            let minimum_size = current_bytes
                .saturating_sub(observation_bytes)
                .saturating_add(minimum_observation_bytes);
            let minimum_estimated_bytes = current_estimated_bytes
                .saturating_sub(observation_estimated_bytes)
                .saturating_add(minimum_observation_estimated_bytes);
            if minimum_size >= current_bytes && minimum_estimated_bytes >= current_estimated_bytes {
                set_compactable_text(observation, text_index, original);
                continue;
            }
            if !fits(minimum_size, minimum_estimated_bytes, budget_bytes) {
                current_bytes = minimum_size;
                current_estimated_bytes = minimum_estimated_bytes;
                observation_bytes = minimum_observation_bytes;
                observation_estimated_bytes = minimum_observation_estimated_bytes;
                compacted_fields += 1;
                omitted_bytes = omitted_bytes.saturating_add(original.len());
                continue;
            }

            let mut lower = 0_usize;
            let mut upper = character_count;
            let mut best = 0_usize;
            while lower <= upper {
                let middle = lower + (upper - lower) / 2;
                set_compactable_text(
                    observation,
                    text_index,
                    compacted_text(&original, &character_boundaries, middle, &digest),
                );
                let candidate_observation_bytes = serialized_observation_size(observation)?;
                let candidate_observation_estimated_bytes = observation.estimated_size();
                let candidate_size = current_bytes
                    .saturating_sub(observation_bytes)
                    .saturating_add(candidate_observation_bytes);
                let candidate_estimated_bytes = current_estimated_bytes
                    .saturating_sub(observation_estimated_bytes)
                    .saturating_add(candidate_observation_estimated_bytes);
                if fits(candidate_size, candidate_estimated_bytes, budget_bytes) {
                    best = middle;
                    lower = middle.saturating_add(1);
                } else if middle == 0 {
                    break;
                } else {
                    upper = middle - 1;
                }
            }

            set_compactable_text(
                observation,
                text_index,
                compacted_text(&original, &character_boundaries, best, &digest),
            );
            let compacted_observation_bytes = serialized_observation_size(observation)?;
            let compacted_observation_estimated_bytes = observation.estimated_size();
            current_bytes = current_bytes
                .saturating_sub(observation_bytes)
                .saturating_add(compacted_observation_bytes);
            current_estimated_bytes = current_estimated_bytes
                .saturating_sub(observation_estimated_bytes)
                .saturating_add(compacted_observation_estimated_bytes);
            observation_bytes = compacted_observation_bytes;
            observation_estimated_bytes = compacted_observation_estimated_bytes;
            let retained_bytes = retained_text_bytes(&original, &character_boundaries, best);
            compacted_fields += 1;
            omitted_bytes =
                omitted_bytes.saturating_add(original.len().saturating_sub(retained_bytes));
        }
        if fits(current_bytes, current_estimated_bytes, budget_bytes) {
            break;
        }
    }

    if !fits(current_bytes, current_estimated_bytes, budget_bytes) {
        return Err(ContextError::ObservationsTooLarge);
    }

    Ok(Some(CompactedObservations {
        observations: compacted,
        report: ObservationCompactionReport {
            original_bytes,
            sent_bytes: current_bytes,
            compacted_fields,
            omitted_bytes,
        },
    }))
}

fn fits(serialized_bytes: usize, estimated_bytes: usize, budget_bytes: usize) -> bool {
    serialized_bytes <= budget_bytes && estimated_bytes <= MAX_OBSERVATION_BYTES
}

fn serialized_observation_bytes<T: ModelObservation>(
    observations: &[T],
) -> Result<usize, ContextError> {
    let mut total_bytes = 2_usize.saturating_add(observations.len().saturating_sub(1));
    for observation in observations {
        total_bytes = total_bytes.saturating_add(serialized_observation_size(observation)?);
    }
    Ok(total_bytes)
}

fn serialized_observation_size<T: ModelObservation>(
    observation: &T,
) -> Result<usize, ContextError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, &observation.to_model_value())
        .map_err(|_| ContextError::RequestEncoding)?;
    Ok(counter.0)
}

#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn estimated_observation_bytes<T: ModelObservation>(observations: &[T]) -> usize {
    observations.iter().fold(0_usize, |total, observation| {
        total.saturating_add(observation.estimated_size())
    })
}

fn set_compactable_text<T: ModelObservation>(observation: &mut T, index: usize, value: String) {
    let Some(slot) = observation.compactable_text_at(index) else {
        unreachable!("text field index is validated by compactable_text_count");
    };
    *slot = value;
}

fn compacted_text(
    original: &str,
    boundaries: &CharacterBoundaries,
    retained_characters: usize,
    digest: &str,
) -> String {
    if retained_characters >= boundaries.character_count {
        return original.to_owned();
    }
    let prefix_characters = retained_characters.div_ceil(2);
    let suffix_characters = retained_characters / 2;
    let prefix_end = boundaries.byte_at_character(original, prefix_characters);
    let suffix_start = boundaries.byte_at_character(
        original,
        boundaries.character_count.saturating_sub(suffix_characters),
    );
    let retained_bytes = prefix_end.saturating_add(original.len().saturating_sub(suffix_start));
    let omitted_bytes = original.len().saturating_sub(retained_bytes);
    let marker = format!(
        "[Krypper context note: text compacted; original_bytes={}; retained_bytes={retained_bytes}; omitted_bytes={omitted_bytes}; sha256={digest}]",
        original.len()
    );
    if retained_characters == 0 {
        marker
    } else {
        format!(
            "{}\n{marker}\n{}",
            &original[..prefix_end],
            &original[suffix_start..]
        )
    }
}

fn retained_text_bytes(
    original: &str,
    boundaries: &CharacterBoundaries,
    retained_characters: usize,
) -> usize {
    let prefix_characters = retained_characters.div_ceil(2);
    let suffix_characters = retained_characters / 2;
    boundaries
        .byte_at_character(original, prefix_characters)
        .saturating_add(
            boundaries
                .byte_len
                .saturating_sub(boundaries.byte_at_character(
                    original,
                    boundaries.character_count.saturating_sub(suffix_characters),
                )),
        )
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_lifecycle::WorkspaceSearchLocation;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn observation_compaction_keeps_source_evidence_separate_and_marks_omissions() {
        let original = vec![WorkspaceObservation::FileRead {
            path: PathBuf::from("src/important.rs"),
            contents: format!(
                "keep-this-header\n{}\nkeep-this-tail",
                "detail\n".repeat(3_000)
            ),
        }];
        let authoritative = original.clone();

        let compacted = compact_observations_for_model(&original, 1_024)
            .expect("compact the model-facing view")
            .expect("oversized evidence should be compacted");
        let encoded = serialized_observation_bytes(&compacted.observations)
            .expect("serialize compacted evidence");
        let rendered = serde_json::to_string(
            &compacted
                .observations
                .iter()
                .map(|observation| observation.to_model_value())
                .collect::<Vec<_>>(),
        )
        .expect("render compacted evidence");

        assert!(encoded <= 1_024);
        assert_eq!(
            compacted.report.original_bytes,
            serialized_observation_bytes(&original).unwrap()
        );
        assert_eq!(compacted.report.sent_bytes, encoded);
        assert_eq!(compacted.report.compacted_fields, 1);
        assert!(compacted.report.omitted_bytes > 0);
        assert!(rendered.contains("src/important.rs"));
        assert!(rendered.contains("Krypper context note: text compacted"));
        assert!(rendered.contains("original_bytes="));
        assert!(rendered.contains("sha256="));
        assert!(rendered.contains("keep-this-header"));
        assert!(rendered.contains("keep-this-tail"));
        assert_eq!(
            original, authoritative,
            "the original evidence must stay untouched"
        );
    }

    #[test]
    fn observation_compaction_preserves_newer_text_and_structured_process_outcomes() {
        let mut process = crate::ProcessResult {
            run_id: uuid::Uuid::nil(),
            outcome: crate::ProcessOutcome::Exited(0),
            profile: crate::ProcessExecutionProfile::OfflineCargoTest,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            diagnostic: None,
            cleanup_confirmed: true,
            test_filter: None,
            tests_ran: false,
            executed_tests: None,
            elapsed_ms: 0,
            output_bytes: 0,
            output_limit: 0,
            resource_events: Vec::new(),
            recovery_hold: false,
            sandbox_evidence: None,
        };
        process.stdout = "older output ".repeat(1_000);
        process.truncated = true;
        let observations = vec![
            WorkspaceObservation::ProcessFinished { result: process },
            WorkspaceObservation::FileRead {
                path: PathBuf::from("src/current.rs"),
                contents: "newest relevant evidence".to_owned(),
            },
        ];
        let authoritative = observations.clone();

        let compacted = compact_observations_for_model(&observations, 2_048)
            .expect("compact the model-facing view")
            .expect("oversized evidence should be compacted");

        let WorkspaceObservation::ProcessFinished { result } = &compacted.observations[0] else {
            panic!("process result keeps its structured event type");
        };
        assert_eq!(result.outcome, crate::ProcessOutcome::Exited(0));
        assert!(
            result.truncated,
            "the original process outcome flag is preserved"
        );
        assert!(result
            .stdout
            .contains("Krypper context note: text compacted"));
        assert!(matches!(
            &compacted.observations[1],
            WorkspaceObservation::FileRead { contents, .. }
                if contents == "newest relevant evidence"
        ));
        assert_eq!(observations, authoritative);
    }

    #[test]
    fn observation_compaction_fails_when_structured_metadata_cannot_fit() {
        let observations = [WorkspaceObservation::DirectoryListed {
            path: PathBuf::from("."),
            entries: ["Cargo.toml", "src", "tests"].map(OsString::from).into(),
        }];

        assert!(matches!(
            compact_observations_for_model(&observations, 16),
            Err(ContextError::ObservationsTooLarge)
        ));
    }

    #[test]
    fn observation_compaction_respects_unicode_boundaries() {
        let original = [WorkspaceObservation::FileRead {
            path: PathBuf::from("src/unicode.rs"),
            contents: format!("start {} end", "🧪".repeat(2_000)),
        }];
        let compacted = compact_observations_for_model(&original, 512)
            .expect("compact UTF-8 text")
            .expect("large text is compacted");
        let WorkspaceObservation::FileRead { contents, .. } = &compacted.observations[0] else {
            panic!("file read remains a file read");
        };
        assert!(contents.is_char_boundary(contents.len()));
        assert!(contents.contains("Krypper context note: text compacted"));
    }

    #[test]
    fn observation_compaction_preserves_uncompactable_search_metadata() {
        let observations = [
            WorkspaceObservation::FileRead {
                path: PathBuf::from("large.txt"),
                contents: "old output ".repeat(2_000),
            },
            WorkspaceObservation::TextSearched {
                directory: PathBuf::from("."),
                locations: vec![WorkspaceSearchLocation {
                    path: PathBuf::from("src/current.rs"),
                    line: 17,
                }],
            },
        ];
        let compacted = compact_observations_for_model(&observations, 1_024)
            .expect("compact without dropping search metadata")
            .expect("large earlier text is compacted");

        assert!(matches!(
            &compacted.observations[1],
            WorkspaceObservation::TextSearched { locations, .. }
                if locations == &[WorkspaceSearchLocation { path: PathBuf::from("src/current.rs"), line: 17 }]
        ));
    }
}
