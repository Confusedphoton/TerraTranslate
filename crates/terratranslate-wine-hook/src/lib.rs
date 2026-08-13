#![cfg_attr(not(windows), allow(dead_code))]

//! Wine-side semantic text interception.
//!
//! The Windows build is a DLL. Its loader entry point does no initialization:
//! [`TerraTranslateHookStartW`] must be called by the injector after `LoadLibraryW`
//! has completed. Hook callbacks only make bounded copies and use `try_send`; all
//! socket and serialization work happens on the bridge worker.

use std::collections::{BTreeMap, BTreeSet};

use terratranslate_wine_protocol::{
    BridgeMessage, ExecutableIdentity, HookCandidate, HookPlatform, HookTextEvent,
    MAX_SAMPLE_BYTES, StableCandidateKey,
};
use uuid::Uuid;

const MAX_CANDIDATES: usize = 512;
pub(crate) const MAX_TEXT_UTF16: usize = 8 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub adapter: &'static str,
    pub api: &'static str,
    pub callsite_module: String,
    pub callsite_offset: u64,
    pub text: String,
    pub thread_id: u32,
    pub timestamp_ms: i64,
}

#[derive(Debug)]
struct CandidateState {
    id: Uuid,
    last_text: Option<String>,
    sample: Observation,
    advertised: bool,
}

/// Connection-local candidate registry. Stable identity is deliberately kept
/// separate from the UUID used for commands on the current connection.
#[derive(Debug)]
pub(crate) struct CandidateBook {
    executable_identity: ExecutableIdentity,
    candidates: BTreeMap<String, CandidateState>,
    enabled: BTreeSet<Uuid>,
    sequence: u64,
}

impl CandidateBook {
    pub(crate) fn new(executable_identity: ExecutableIdentity) -> Self {
        Self {
            executable_identity,
            candidates: BTreeMap::new(),
            enabled: BTreeSet::new(),
            sequence: 0,
        }
    }

    pub(crate) fn set_enabled(&mut self, id: Uuid, enabled: bool) {
        if enabled {
            self.enabled.insert(id);
        } else {
            self.enabled.remove(&id);
        }
    }

    pub(crate) fn disable_all(&mut self) {
        self.enabled.clear();
    }

    /// Starts a new command namespace after transport reconnection. Stable keys
    /// remain unchanged, but the host must receive fresh connection-local UUIDs.
    pub(crate) fn reset_connection(&mut self) {
        self.enabled.clear();
        for candidate in self.candidates.values_mut() {
            candidate.id = Uuid::new_v4();
            candidate.last_text = None;
            candidate.advertised = false;
        }
    }

    pub(crate) fn advertisements(&mut self) -> Vec<BridgeMessage> {
        self.candidates
            .iter_mut()
            .filter_map(|(stable_key, state)| {
                if state.advertised {
                    return None;
                }
                state.advertised = true;
                Some(candidate_message(
                    stable_key,
                    state.id,
                    state.sample.clone(),
                ))
            })
            .collect()
    }

    pub(crate) fn observe(&mut self, observation: Observation) -> Vec<BridgeMessage> {
        if observation.text.is_empty() {
            return Vec::new();
        }
        let stable_key = StableCandidateKey::derive(
            &HookPlatform::Windows,
            &self.executable_identity,
            observation.adapter,
            Some(&observation.callsite_module),
            Some(observation.callsite_offset),
        );
        let stable_key_string = stable_key.to_string();
        let is_new = !self.candidates.contains_key(&stable_key_string);
        if is_new && self.candidates.len() >= MAX_CANDIDATES {
            return Vec::new();
        }
        let state = self
            .candidates
            .entry(stable_key_string)
            .or_insert_with(|| CandidateState {
                id: Uuid::new_v4(),
                last_text: None,
                sample: observation.clone(),
                advertised: false,
            });

        let mut messages = Vec::with_capacity(2);
        if is_new {
            state.advertised = true;
            messages.push(candidate_message(
                stable_key.as_str(),
                state.id,
                observation.clone(),
            ));
        }

        // Discovery samples may be sent while disabled, but routable text may not.
        if !self.enabled.contains(&state.id)
            || state.last_text.as_deref() == Some(observation.text.as_str())
        {
            return messages;
        }
        state.last_text = Some(observation.text.clone());
        self.sequence = self.sequence.wrapping_add(1);
        messages.push(BridgeMessage::Text(HookTextEvent {
            sequence: self.sequence,
            candidate_id: state.id,
            stable_key,
            thread_id: observation.thread_id,
            timestamp_ms: observation.timestamp_ms,
            text: observation.text,
            speaker: None,
            replacement_capacity_utf16: None,
        }));
        messages
    }
}

fn candidate_message(stable_key: &str, id: Uuid, observation: Observation) -> BridgeMessage {
    BridgeMessage::Candidate(HookCandidate {
        candidate_id: id,
        stable_key: StableCandidateKey::new(stable_key).expect("derived stable key is valid"),
        adapter_id: observation.adapter.into(),
        api: observation.api.into(),
        caller_module: Some(observation.callsite_module),
        module_offset: Some(observation.callsite_offset),
        sample: bounded_utf8(observation.text, MAX_SAMPLE_BYTES),
        embeddable: false,
        metadata: BTreeMap::new(),
    })
}

pub(crate) fn bounded_utf8(mut text: String, maximum: usize) -> String {
    if text.len() > maximum {
        let mut end = maximum;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

pub(crate) fn stable_candidate_key(
    executable_identity: &ExecutableIdentity,
    adapter: &str,
    callsite_module: &str,
    callsite_offset: u64,
) -> StableCandidateKey {
    StableCandidateKey::derive(
        &HookPlatform::Windows,
        executable_identity,
        adapter,
        Some(callsite_module),
        Some(callsite_offset),
    )
}

pub(crate) unsafe fn bounded_utf16(pointer: *const u16, units: usize) -> Option<String> {
    if pointer.is_null() || units == 0 {
        return None;
    }
    let units = units.min(MAX_TEXT_UTF16);
    // SAFETY: API contracts guarantee `pointer` is readable for `units`; the caller
    // invokes this before calling the original rendering function.
    let slice = unsafe { std::slice::from_raw_parts(pointer, units) };
    Some(
        String::from_utf16_lossy(slice)
            .trim_matches('\0')
            .to_owned(),
    )
}

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{DllMain, TerraTranslateHookStartW};

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(text: &str) -> Observation {
        Observation {
            adapter: "gdi",
            api: "TextOutW",
            callsite_module: "game.exe".into(),
            callsite_offset: 0x1234,
            text: text.into(),
            thread_id: 7,
            timestamp_ms: 10,
        }
    }

    #[test]
    fn disabled_candidate_only_emits_discovery_sample() {
        let mut book = CandidateBook::new(executable());
        let messages = book.observe(observation("こんにちは"));
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0], BridgeMessage::Candidate(_)));
    }

    #[test]
    fn enabled_candidate_emits_text_and_suppresses_consecutive_repeat() {
        let mut book = CandidateBook::new(executable());
        let discovered = book.observe(observation("one"));
        let BridgeMessage::Candidate(candidate) = &discovered[0] else {
            panic!("expected candidate")
        };
        book.set_enabled(candidate.candidate_id, true);
        assert!(matches!(
            book.observe(observation("one")).as_slice(),
            [BridgeMessage::Text(_)]
        ));
        assert!(book.observe(observation("one")).is_empty());
        assert!(matches!(
            book.observe(observation("two")).as_slice(),
            [BridgeMessage::Text(_)]
        ));
    }

    #[test]
    fn stable_key_ignores_connection_uuid() {
        let executable = executable();
        assert_eq!(
            stable_candidate_key(&executable, "gdi", "dialogue.dll", 12),
            stable_candidate_key(&executable, "gdi", "dialogue.dll", 12)
        );
        assert_ne!(
            stable_candidate_key(&executable, "gdi", "dialogue.dll", 12),
            stable_candidate_key(&executable, "gdi", "dialogue.dll", 13)
        );
    }

    fn executable() -> ExecutableIdentity {
        ExecutableIdentity {
            path: "C:\\game.exe".into(),
            image_id: Some("sha256:game".into()),
        }
    }

    #[test]
    fn utf16_copy_is_bounded() {
        let text = vec![b'a' as u16; MAX_TEXT_UTF16 + 100];
        let copied = unsafe { bounded_utf16(text.as_ptr(), text.len()) }.unwrap();
        assert_eq!(copied.len(), MAX_TEXT_UTF16);
    }

    #[test]
    fn reconnect_reissues_candidate_with_new_local_id() {
        let mut book = CandidateBook::new(executable());
        let discovered = book.observe(observation("sample"));
        let BridgeMessage::Candidate(before) = &discovered[0] else {
            panic!("expected candidate")
        };
        let old_id = before.candidate_id;
        let stable_key = before.stable_key.clone();
        book.reset_connection();
        let advertisements = book.advertisements();
        let BridgeMessage::Candidate(after) = &advertisements[0] else {
            panic!("expected candidate")
        };
        assert_ne!(after.candidate_id, old_id);
        assert_eq!(after.stable_key, stable_key);
    }
}
