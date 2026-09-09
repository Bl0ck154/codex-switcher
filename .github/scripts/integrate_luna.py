from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    if old not in text:
        raise SystemExit(f"Expected patch anchor not found in {path}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1))


# Rust wire types: keep Reserve optional for backward-compatible payloads.
replace_once(
    "src-tauri/src/types.rs",
    "/// Usage information for an account\n#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct UsageInfo {",
    """/// GPT/Luna Reserve quota exposed by ChatGPT as the `gpt-reserve` additional rate limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LunaReserveInfo {
    pub normal_model_slug: String,
    pub allowed: bool,
    pub limit_reached: bool,
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

/// Usage information for an account
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {""",
)
replace_once(
    "src-tauri/src/types.rs",
    "    /// Whether the account has credits\n    pub has_credits: Option<bool>,",
    """    /// Optional GPT/Luna Reserve bucket. Present only when OpenAI exposes `gpt-reserve`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub luna_reserve: Option<LunaReserveInfo>,
    /// Whether the account has credits
    pub has_credits: Option<bool>,""",
)
replace_once(
    "src-tauri/src/types.rs",
    "            secondary_resets_at: None,\n            has_credits: None,",
    "            secondary_resets_at: None,\n            luna_reserve: None,\n            has_credits: None,",
)
replace_once(
    "src-tauri/src/types.rs",
    "    #[serde(default)]\n    pub rate_limit: Option<RateLimitDetails>,\n    #[serde(default)]\n    pub credits: Option<CreditStatusDetails>,",
    """    #[serde(default)]
    pub rate_limit: Option<RateLimitDetails>,
    /// Model-specific/additional quota buckets such as OpenAI's `gpt-reserve`.
    #[serde(default)]
    pub additional_rate_limits: Vec<serde_json::Value>,
    #[serde(default)]
    pub credits: Option<CreditStatusDetails>,""",
)

# Backend usage parser.
replace_once(
    "src-tauri/src/api/usage.rs",
    "    AuthData, CreditStatusDetails, RateLimitDetails, RateLimitStatusPayload, RateLimitWindow,\n    StoredAccount, UsageInfo,",
    "    AuthData, CreditStatusDetails, LunaReserveInfo, RateLimitDetails, RateLimitStatusPayload,\n    RateLimitWindow, StoredAccount, UsageInfo,",
)
replace_once(
    "src-tauri/src/api/usage.rs",
    "            secondary_resets_at: None,\n            has_credits: None,",
    "            secondary_resets_at: None,\n            luna_reserve: None,\n            has_credits: None,",
)
replace_once(
    "src-tauri/src/api/usage.rs",
    "fn convert_payload_to_usage_info(account_id: &str, payload: RateLimitStatusPayload) -> UsageInfo {\n    let (primary, secondary) = extract_rate_limits(payload.rate_limit);\n    let credits = extract_credits(payload.credits);",
    "fn convert_payload_to_usage_info(account_id: &str, payload: RateLimitStatusPayload) -> UsageInfo {\n    let luna_reserve = extract_luna_reserve(&payload.additional_rate_limits);\n    let (primary, secondary) = extract_rate_limits(payload.rate_limit);\n    let credits = extract_credits(payload.credits);",
)
replace_once(
    "src-tauri/src/api/usage.rs",
    "        secondary_resets_at: secondary.as_ref().and_then(|w| w.reset_at),\n        has_credits: credits.as_ref().map(|c| c.has_credits),",
    "        secondary_resets_at: secondary.as_ref().and_then(|w| w.reset_at),\n        luna_reserve,\n        has_credits: credits.as_ref().map(|c| c.has_credits),",
)
replace_once(
    "src-tauri/src/api/usage.rs",
    "fn extract_rate_limits(\n    rate_limit: Option<RateLimitDetails>,",
    """fn json_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
    })
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
            .or_else(|| value.as_f64().map(|number| number as i64))
            .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
    })
}

fn extract_luna_reserve(additional_rate_limits: &[Value]) -> Option<LunaReserveInfo> {
    let entry = additional_rate_limits.iter().find(|entry| {
        entry
            .get("limit_name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("gpt-reserve"))
    })?;
    let rate_limit = entry.get("rate_limit")?;
    let primary = rate_limit.get("primary_window")?;
    let used_percent = json_number(primary.get("used_percent"))?.clamp(0.0, 100.0);
    let reset_after_seconds = json_i64(primary.get("reset_after_seconds"));
    let resets_at = json_i64(primary.get("reset_at")).or_else(|| {
        reset_after_seconds
            .filter(|seconds| *seconds > 0)
            .map(|seconds| Utc::now().timestamp() + seconds)
    });
    let window_minutes = json_i64(primary.get("limit_window_seconds"))
        .filter(|seconds| *seconds > 0)
        .map(|seconds| (seconds + 59) / 60);

    Some(LunaReserveInfo {
        normal_model_slug: entry
            .get("normal_model_slug")
            .and_then(Value::as_str)
            .unwrap_or("gpt-5.6-luna")
            .to_string(),
        allowed: rate_limit
            .get("allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limit_reached: rate_limit
            .get("limit_reached")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        used_percent,
        window_minutes,
        resets_at,
    })
}

fn extract_rate_limits(
    rate_limit: Option<RateLimitDetails>,""",
)

# Regression coverage for the exact OpenAI gpt-reserve shape and safe filtering.
replace_once(
    "src-tauri/src/api/usage.rs",
    """    #[test]
    fn preserves_unknown_windows_by_backend_position() {
        let (primary, secondary) = extract_rate_limits(Some(RateLimitDetails {
            primary_window: Some(rate_limit_window(11.0, 60 * 60)),
            secondary_window: Some(rate_limit_window(22.0, 30 * 24 * 60 * 60)),
        }));

        assert_eq!(primary.map(|window| window.used_percent), Some(11.0));
        assert_eq!(secondary.map(|window| window.used_percent), Some(22.0));
    }
}""",
    """    #[test]
    fn preserves_unknown_windows_by_backend_position() {
        let (primary, secondary) = extract_rate_limits(Some(RateLimitDetails {
            primary_window: Some(rate_limit_window(11.0, 60 * 60)),
            secondary_window: Some(rate_limit_window(22.0, 30 * 24 * 60 * 60)),
        }));

        assert_eq!(primary.map(|window| window.used_percent), Some(11.0));
        assert_eq!(secondary.map(|window| window.used_percent), Some(22.0));
    }

    #[test]
    fn parses_gpt_reserve_additional_rate_limit() {
        let payload = serde_json::json!({
            "limit_name": "gpt-reserve",
            "normal_model_slug": "gpt-5.6-luna",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 37.5,
                    "limit_window_seconds": 604800,
                    "reset_at": 1_800_000_000
                }
            }
        });
        let reserve = extract_luna_reserve(&[payload]).expect("reserve should parse");
        assert_eq!(reserve.normal_model_slug, "gpt-5.6-luna");
        assert!(reserve.allowed);
        assert!(!reserve.limit_reached);
        assert_eq!(reserve.used_percent, 37.5);
        assert_eq!(reserve.window_minutes, Some(10080));
        assert_eq!(reserve.resets_at, Some(1_800_000_000));
    }

    #[test]
    fn ignores_non_reserve_additional_limits() {
        let payload = serde_json::json!({
            "limit_name": "GPT-5.3-Codex-Spark",
            "rate_limit": { "primary_window": { "used_percent": 25 } }
        });
        assert!(extract_luna_reserve(&[payload]).is_none());
    }
}""",
)

# Frontend wire type.
replace_once(
    "src/types/index.ts",
    "export interface UsageInfo {",
    """export interface LunaReserveInfo {
  normal_model_slug: string;
  allowed: boolean;
  limit_reached: boolean;
  used_percent: number;
  window_minutes: number | null;
  resets_at: number | null;
}

export interface UsageInfo {""",
)
replace_once(
    "src/types/index.ts",
    "  secondary_resets_at: number | null;\n  has_credits: boolean | null;",
    "  secondary_resets_at: number | null;\n  luna_reserve?: LunaReserveInfo | null;\n  has_credits: boolean | null;",
)

# Display Reserve as a third independent quota bar without conflating it with 5h/weekly.
replace_once(
    "src/components/UsageBar.tsx",
    "  const hasSecondary = usage.secondary_used_percent !== null && usage.secondary_used_percent !== undefined;\n\n  if (!hasPrimary && !hasSecondary) {",
    "  const hasSecondary = usage.secondary_used_percent !== null && usage.secondary_used_percent !== undefined;\n  const lunaReserve = usage.luna_reserve ?? null;\n\n  if (!hasPrimary && !hasSecondary && !lunaReserve) {",
)
replace_once(
    "src/components/UsageBar.tsx",
    """      {hasSecondary && (
        <RateLimitBar
          label="Weekly Limit"
          usedPercent={usage.secondary_used_percent!}
          windowMinutes={usage.secondary_window_minutes}
          resetsAt={usage.secondary_resets_at}
        />
      )}
      {usage.credits_balance && (""",
    """      {hasSecondary && (
        <RateLimitBar
          label="Weekly Limit"
          usedPercent={usage.secondary_used_percent!}
          windowMinutes={usage.secondary_window_minutes}
          resetsAt={usage.secondary_resets_at}
        />
      )}
      {lunaReserve && (
        <div className="space-y-1">
          <RateLimitBar
            label={`GPT Reserve • ${lunaReserve.normal_model_slug || "Luna"}`}
            usedPercent={lunaReserve.used_percent}
            resetsAt={lunaReserve.resets_at}
          />
          <div className="text-[11px] text-gray-500 dark:text-gray-400">
            {lunaReserve.allowed && !lunaReserve.limit_reached && lunaReserve.used_percent < 100
              ? "Luna fallback available when normal Codex quota is exhausted"
              : "Luna Reserve unavailable or exhausted"}
          </div>
        </div>
      )}
      {usage.credits_balance && (""",
)
