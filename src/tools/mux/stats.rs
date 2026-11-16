//! Claude Code's own statistics (what its `/stats` shows), read from the cache it keeps
//! in ~/.claude/stats-cache.json: every use, in the terminal as in Akolade.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CcStats {
    /// The day Claude Code last computed them ("2026-11-10").
    pub last_computed_date: String,
    pub first_session_date: String,
    pub total_sessions: u64,
    pub total_messages: u64,
    pub daily_activity: Vec<DayActivity>,
    pub model_usage: BTreeMap<String, ModelUsage>,
    pub hour_counts: BTreeMap<String, u64>,
    pub longest_session: Longest,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DayActivity {
    pub date: String,
    pub message_count: u64,
    pub session_count: u64,
    pub tool_call_count: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl ModelUsage {
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Longest {
    /// In milliseconds.
    pub duration: u64,
    pub message_count: u64,
}

impl CcStats {
    /// None: Claude Code has not computed any yet (never used, or `/stats` never run).
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(crate::platform::home().join(".claude").join("stats-cache.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The hour of the day with the most sessions started.
    pub fn favorite_hour(&self) -> Option<u32> {
        self.hour_counts.iter().max_by_key(|(_, n)| **n).and_then(|(h, _)| h.parse().ok())
    }
}

/// "2026-11-10" (or a timestamp) → "10/11/2026".
pub fn fr_date(date: &str) -> String {
    match date.get(..10).map(|d| d.split('-').collect::<Vec<_>>()).as_deref() {
        Some([y, m, d]) => format!("{d}/{m}/{y}"),
        _ => date.to_string(),
    }
}

/// Milliseconds → "2 h 05 min", "18 min".
pub fn duration(ms: u64) -> String {
    let min = ms / 60_000;
    if min < 60 { format!("{min} min") } else { format!("{} h {:02} min", min / 60, min % 60) }
}

/// The `n` days up to `last` (its "2026-11-10"), each with its activity (none: zeros).
pub fn last_days(s: &CcStats, n: i64) -> Vec<(String, u64, u64)> {
    let Ok(last) = chrono::NaiveDate::parse_from_str(&s.last_computed_date, "%Y-%m-%d") else { return vec![] };
    (0..n)
        .rev()
        .map(|ago| (last - chrono::Duration::days(ago)).format("%Y-%m-%d").to_string())
        .map(|d| {
            let a = s.daily_activity.iter().find(|a| a.date == d);
            let (m, t) = a.map_or((0, 0), |a| (a.message_count, a.tool_call_count));
            (d, m, t)
        })
        .collect()
}

/// "claude-opus-5-5" → "Opus 5.5" ("claude-sonnet-4-5-20250929" → "Sonnet 4.5").
pub fn model_label(id: &str) -> String {
    let mut parts = id.trim_start_matches("claude-").split('-').filter(|p| p.len() < 8);
    let Some(family) = parts.next() else { return id.to_string() };
    let mut name: String = family.chars().take(1).flat_map(char::to_uppercase).chain(family.chars().skip(1)).collect();
    let version: Vec<&str> = parts.collect();
    if !version.is_empty() {
        name = format!("{name} {}", version.join("."));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claude_codes_cache_and_names_its_models() {
        let s: CcStats = serde_json::from_str(r#"{"version":5,"lastComputedDate":"2026-11-10","dailyActivity":[{"date":"2026-11-02","messageCount":2243,"sessionCount":29,"toolCallCount":613}],"modelUsage":{"claude-opus-5-5":{"inputTokens":1,"outputTokens":2,"cacheReadInputTokens":3,"cacheCreationInputTokens":4,"costUSD":0}},"totalSessions":43,"totalMessages":9195,"longestSession":{"sessionId":"x","duration":7200000,"messageCount":599},"firstSessionDate":"2026-10-17T18:06:24.479Z","hourCounts":{"9":2,"13":30}}"#).unwrap();
        assert_eq!((s.total_sessions, s.daily_activity[0].message_count, s.model_usage["claude-opus-5-5"].tokens()), (43, 2243, 10));
        assert_eq!((s.favorite_hour(), s.longest_session.duration), (Some(13), 7_200_000));
        assert_eq!(model_label("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_label("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_label("claude-haiku"), "Haiku");
        assert_eq!((fr_date("2026-10-17T18:06:24.479Z"), duration(7_500_000), duration(60_000)), ("17/10/2026".into(), "2 h 05 min".into(), "1 min".into()));
        let days = last_days(&s, 9);
        assert_eq!((days.len(), days[0].clone(), days[8].1), (9, ("2026-11-02".into(), 2243, 613), 0));
    }
}
