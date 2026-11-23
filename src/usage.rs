//! Quota bookkeeping: what each turn used (tokens, and dollars at list price when the agent says), and each
//! subscription's latest quota windows. Only what the agents report themselves; nothing here calls a model.
//! Turns go to usage.jsonl in the data folder, one line per model per turn, kept within `usage_keep_days` and
//! `usage_max_mb`; the windows go to state.json under "quota".
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

/// One model's use in one turn, as the agent reported it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Use { pub model: String, pub input: u64, pub output: u64, pub cache_read: u64, pub cache_write: u64, pub usd: Option<f64> }

impl Use {
    fn tokens(&self) -> u64 { self.input + self.output + self.cache_read + self.cache_write }
    fn add(&mut self, o: &Use) {
        self.input += o.input; self.output += o.output; self.cache_read += o.cache_read; self.cache_write += o.cache_write;
        if let Some(u) = o.usd { self.usd = Some(self.usd.unwrap_or(0.0) + u); }
    }
}

/// A quota window: its name ("5h", "7d", or the agent's own), status ("allowed", "warning", "rejected"),
/// the share used (0–1) and when it resets (unix seconds), as far as the agent said.
#[derive(Clone, Debug, PartialEq)]
pub struct Window { pub name: String, pub status: String, pub used: Option<f64>, pub resets: Option<i64> }

// ---- what the agents report ----

/// claude's result event: the use since the previous result, per model (it also carries running totals, not used).
pub fn from_claude_result(v: &Value) -> Vec<Use> {
    let per = &v["model_usage_since_last_result"];
    if let Some(m) = per.as_object().filter(|m| !m.is_empty()) {
        return m.iter().map(|(name, u)| Use {
            model: name.clone(),
            input: u["inputTokens"].as_u64().unwrap_or(0), output: u["outputTokens"].as_u64().unwrap_or(0),
            cache_read: u["cacheReadInputTokens"].as_u64().unwrap_or(0), cache_write: u["cacheCreationInputTokens"].as_u64().unwrap_or(0),
            usd: u["costUSD"].as_f64(),
        }).filter(|u| u.tokens() > 0).collect();
    }
    // older claude: this turn's main-model usage only, model unknown
    let u = &v["usage"];
    let one = Use { model: "?".into(), input: u["input_tokens"].as_u64().unwrap_or(0), output: u["output_tokens"].as_u64().unwrap_or(0),
                    cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0), cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0), usd: None };
    if one.tokens() > 0 { vec![one] } else { vec![] }
}

/// pi's turn_end message: one model call's usage (pi gives the cost too).
pub fn from_pi(msg: &Value) -> Option<Use> {
    let u = &msg["usage"];
    let model = [msg["provider"].as_str(), msg["model"].as_str()].into_iter().flatten().collect::<Vec<_>>().join("/");
    let one = Use { model: if model.is_empty() { "?".into() } else { model },
                    input: u["input"].as_u64().unwrap_or(0), output: u["output"].as_u64().unwrap_or(0),
                    cache_read: u["cacheRead"].as_u64().unwrap_or(0), cache_write: u["cacheWrite"].as_u64().unwrap_or(0),
                    usd: u["cost"]["total"].as_f64().filter(|c| *c > 0.0) };
    (one.tokens() > 0).then_some(one)
}

/// codex's thread/tokenUsage/updated "last" breakdown: one model call (its input counts the cached part too).
pub fn from_codex(last: &Value, model: &str) -> Option<Use> {
    let input = last["inputTokens"].as_u64().unwrap_or(0);
    let cached = last["cachedInputTokens"].as_u64().unwrap_or(0);
    let one = Use { model: if model.is_empty() { "?".into() } else { model.into() }, input: input.saturating_sub(cached),
                    output: last["outputTokens"].as_u64().unwrap_or(0), cache_read: cached,
                    cache_write: last["cacheWriteInputTokens"].as_u64().unwrap_or(0), usd: None };
    (one.tokens() > 0).then_some(one)
}

/// What one agy turn used (ACP session/prompt result; inputTokens leaves out the cached part, thinking counts as output).
pub fn from_agy_acp(u: &Value, model: &str) -> Option<Use> {
    let g = |k: &str| u[k].as_u64().unwrap_or(0);
    let one = Use { model: if model.is_empty() { "agy".into() } else { model.into() }, input: g("inputTokens"),
                    output: g("outputTokens") + g("thoughtTokens"), cache_read: g("cachedReadTokens"), cache_write: g("cachedWriteTokens"), usd: None };
    (one.tokens() > 0).then_some(one)
}

/// What `agy -p /usage` prints, one window a line: group, window, share remaining, reset time (ISO, UTC), tab-separated.
pub fn agy_windows(out: &str) -> Vec<Window> {
    out.lines().filter_map(|l| {
        let f: Vec<&str> = l.split('\t').map(str::trim).collect();
        if f.len() < 4 { return None; }
        let left = f[2].trim_end_matches('%').parse::<f64>().ok()? / 100.0;
        let span = if f[1].contains("Five Hour") { "5h" } else if f[1].contains("Weekly") { "7d" } else { f[1] };
        let group = f[0].replace(" models", "").replace(" Models", "");
        let used = (1.0 - left).max(0.0);
        Some(Window { name: format!("{group} {span}"), status: if left <= 0.0 { "rejected" } else { "allowed" }.into(), used: Some(used), resets: iso_secs(f[3]) })
    }).collect()
}

/// "2026-11-30T07:52:57Z" as unix seconds.
fn iso_secs(t: &str) -> Option<i64> {
    let n = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Add one call's use to a turn's, per model.
pub fn merge(into: &mut Vec<Use>, u: Use) {
    match into.iter_mut().find(|x| x.model == u.model) { Some(x) => x.add(&u), None => into.push(u) }
}

fn window_name(n: &str) -> String {
    match n { "five_hour" => "5h".into(), "seven_day" => "7d".into(), _ => n.replace("seven_day_", "7d·").replace("five_hour_", "5h·") }
}

/// claude's rate_limit_event: every window in unifiedWindows (else the one the event is about).
pub fn claude_windows(info: &Value) -> Vec<Window> {
    let status = |s: &str| match s { "allowed_warning" => "warning", "rejected" => "rejected", _ => "allowed" }.to_string();
    if let Some(m) = info["unifiedWindows"].as_object().filter(|m| !m.is_empty()) {
        return m.iter().map(|(n, w)| Window { name: window_name(n), status: status(w["status"].as_str().unwrap_or("")),
                                                 used: w["utilization"].as_f64(), resets: w["resetsAt"].as_i64() }).collect();
    }
    match info["rateLimitType"].as_str() {
        Some(n) => vec![Window { name: window_name(n), status: status(info["status"].as_str().unwrap_or("")), used: info["utilization"].as_f64(), resets: info["resetsAt"].as_i64() }],
        None => vec![],
    }
}

/// codex's rate limits: the primary and secondary windows.
pub fn codex_windows(r: &Value) -> Vec<Window> {
    let reached = r["rateLimitReachedType"].as_str().is_some();
    ["primary", "secondary"].iter().filter(|k| r[**k].is_object()).map(|k| {
        let w = &r[*k];
        let name = match w["windowDurationMins"].as_i64() { Some(300) => "5h".into(), Some(10080) => "7d".into(), Some(m) if m % 1440 == 0 => format!("{}d", m / 1440), Some(m) if m % 60 == 0 => format!("{}h", m / 60), Some(m) => format!("{m}m"), None => k.to_string() };
        let used = w["usedPercent"].as_f64().map(|p| p / 100.0);
        let status = if reached && used.unwrap_or(0.0) >= 1.0 { "rejected" } else { "allowed" }.to_string();
        Window { name, status, used, resets: w["resetsAt"].as_i64() }
    }).collect()
}

// ---- keeping them ----

const FILE: &str = "usage.jsonl";
static LOCK: Mutex<()> = Mutex::new(());
static PRUNED: Mutex<u64> = Mutex::new(0);

/// Write down one turn's use.
pub fn record(topic: &str, dev: &str, agent: &str, acct: &str, uses: &[Use]) {
    let now = crate::store::now();
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = crate::store::path(FILE);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        for u in uses {
            let mut v = json!({"t": now, "topic": topic, "dev": dev, "agent": agent, "acct": acct, "model": u.model,
                               "in": u.input, "out": u.output, "cr": u.cache_read, "cw": u.cache_write});
            if let Some(d) = u.usd { v["usd"] = json!((d * 1e6).round() / 1e6); }
            let _ = writeln!(f, "{v}");
        }
    }
    // old lines go once an hour, or at once when the file is over its size
    let max = crate::params::num("usage_max_mb") * 1_000_000;
    let big = std::fs::metadata(&p).map(|m| m.len() > max).unwrap_or(false);
    let mut last = PRUNED.lock().unwrap_or_else(|e| e.into_inner());
    if big || now >= *last + 3600 {
        *last = now;
        if let Ok(s) = std::fs::read_to_string(&p) {
            let kept = prune(&s, now.saturating_sub(crate::params::num("usage_keep_days") * 86400), max as usize);
            if kept.len() != s.len() {
                let tmp = format!("{p}.tmp");
                if std::fs::write(&tmp, kept).is_ok() { let _ = std::fs::rename(tmp, &p); }
            }
        }
    }
}

/// The lines newer than `since`; if those are still over `max` bytes, only the newest half of `max`.
fn prune(s: &str, since: u64, max: usize) -> String {
    let fresh: Vec<&str> = s.lines().filter(|l| serde_json::from_str::<Value>(l).ok().and_then(|v| v["t"].as_u64()).is_some_and(|t| t >= since)).collect();
    let mut size: usize = fresh.iter().map(|l| l.len() + 1).sum();
    let mut from = 0;
    if size > max { while from < fresh.len() && size > max / 2 { size -= fresh[from].len() + 1; from += 1; } }
    fresh[from..].iter().map(|l| format!("{l}\n")).collect()
}

/// A subscription's quota windows, as last reported.
pub fn set_windows(acct: &str, ws: &[Window]) {
    if acct.is_empty() || ws.is_empty() { return; }
    let mut q = crate::store::state("quota");
    if !q.is_object() { q = json!({}); }
    q[acct] = json!({"at": crate::store::now(), "windows": ws.iter().map(|w| json!({"name": w.name, "status": w.status, "used": w.used, "resets": w.resets})).collect::<Vec<_>>()});
    crate::store::set_state("quota", q);
}

// ---- the summary ----

/// "5h", "24h", "7d", "90m" (or nothing: 24 hours) as seconds.
pub fn span(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() { return Some(86400); }
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().ok().filter(|n| *n > 0)?;
    match unit { "m" => Some(n * 60), "h" | "" => Some(n * 3600), "d" => Some(n * 86400), _ => None }
}

/// One line of the summary: a topic or a subscription with what it used.
struct Row { name: String, turns: u64, used: Use }

fn rank(lines: &[Value], key: &str) -> Vec<Row> {
    let mut m: HashMap<String, Row> = HashMap::new();
    for v in lines {
        let name = v[key].as_str().unwrap_or("?").to_string();
        let r = m.entry(name.clone()).or_insert_with(|| Row { name, turns: 0, used: Use::default() });
        r.turns += 1;
        r.used.add(&Use { model: String::new(), input: v["in"].as_u64().unwrap_or(0), output: v["out"].as_u64().unwrap_or(0),
                          cache_read: v["cr"].as_u64().unwrap_or(0), cache_write: v["cw"].as_u64().unwrap_or(0), usd: v["usd"].as_f64() });
    }
    let mut rows: Vec<Row> = m.into_values().collect();
    // dollars first where the agent gave them (claude, pi), then tokens
    rows.sort_by(|a, b| b.used.usd.unwrap_or(0.0).total_cmp(&a.used.usd.unwrap_or(0.0)).then(b.used.tokens().cmp(&a.used.tokens())).then(a.name.cmp(&b.name)));
    rows
}

fn tok(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) } else if n >= 1000 { format!("{:.0}k", n as f64 / 1e3) } else { n.to_string() }
}

fn usd(u: Option<f64>) -> String { u.map(|d| format!("${d:.2}")).unwrap_or_else(|| "—".into()) }

fn ago(secs: i64) -> String { if secs >= 3600 { format!("{}时{}分", secs / 3600, secs % 3600 / 60) } else { format!("{}分", (secs / 60).max(1)) } }

/// One window as a short phrase: "28% 正常，2时15分后重置".
fn window_text(w: &Value, now: i64) -> String {
    let resets = w["resets"].as_i64();
    if resets.is_some_and(|r| r <= now) { return "已重置".into(); }
    let st = match w["status"].as_str() { Some("rejected") => "已满", Some("warning") => "接近上限", _ => "正常" };
    let used = w["used"].as_f64().map(|u| format!("{:.0}% ", u * 100.0)).unwrap_or_default();
    let warn = crate::params::num("limit_warn_pct") as f64 / 100.0;
    let st = if st == "正常" && w["used"].as_f64().is_some_and(|u| u >= warn) { "接近上限" } else { st };
    match resets { Some(r) => format!("{used}{st}，{}后重置", ago(r - now)), None => format!("{used}{st}") }
}

/// The table of each subscription's windows (5h and 7d first), most used 5h first.
fn windows_table(q: &Value, now: i64) -> Option<String> {
    let mut accts: Vec<(&String, &Value)> = q.as_object()?.iter().collect();
    if accts.is_empty() { return None; }
    let used = |a: &Value, n: &str| a["windows"].as_array().into_iter().flatten().find(|w| w["name"] == n)
        .filter(|w| !w["resets"].as_i64().is_some_and(|r| r <= now)).and_then(|w| w["used"].as_f64()).unwrap_or(0.0);
    accts.sort_by(|a, b| used(b.1, "5h").total_cmp(&used(a.1, "5h")).then(a.0.cmp(b.0)));
    let mut names: Vec<String> = vec!["5h".into(), "7d".into()];
    for (_, a) in &accts { for w in a["windows"].as_array().into_iter().flatten() { if let Some(n) = w["name"].as_str() { if !names.iter().any(|x| x == n) { names.push(n.into()); } } } }
    let mut t = format!("| 订阅 | {} | 更新于 |\n|---|{}---|\n", names.join(" | "), "---|".repeat(names.len()));
    for (acct, a) in accts {
        let cells: Vec<String> = names.iter().map(|n| a["windows"].as_array().into_iter().flatten().find(|w| w["name"] == n.as_str()).map(|w| window_text(w, now)).unwrap_or_else(|| "—".into())).collect();
        t += &format!("| {acct} | {} | {}前 |\n", cells.join(" | "), ago(now - a["at"].as_i64().unwrap_or(now)));
    }
    Some(t)
}

fn table(rows: &[Row], first: &str, top: usize) -> String {
    let mut t = format!("| {first} | 轮 | 折算 | 新输入 | 缓存读 | 输出 |\n|---|---|---|---|---|---|\n");
    for r in rows.iter().take(top) {
        t += &format!("| {} | {} | {} | {} | {} | {} |\n", r.name.replace('|', "/"), r.turns, usd(r.used.usd), tok(r.used.input + r.used.cache_write), tok(r.used.cache_read), tok(r.used.output));
    }
    if rows.len() > top {
        let mut rest = Use::default();
        let turns: u64 = rows[top..].iter().map(|r| { rest.add(&r.used); r.turns }).sum();
        t += &format!("| 其余 {} 个 | {turns} | {} | {} | {} | {} |\n", rows.len() - top, usd(rest.usd), tok(rest.input + rest.cache_write), tok(rest.cache_read), tok(rest.output));
    }
    t
}

/// The summary for the last `secs` seconds, as Markdown: the subscriptions' windows first, then totals and the
/// ranking by topic and by subscription.
pub fn report(secs: u64) -> String {
    let lines: Vec<Value> = {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::read_to_string(crate::store::path(FILE)).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    };
    summary(&lines, &crate::store::state("quota"), crate::store::now(), secs)
}

fn span_name(secs: u64) -> String {
    if secs % 86400 == 0 && secs > 2 * 86400 { format!("{} 天", secs / 86400) } else if secs % 3600 == 0 { format!("{} 小时", secs / 3600) } else { format!("{} 分钟", secs / 60) }
}

fn summary(lines: &[Value], quota: &Value, now: u64, secs: u64) -> String {
    let mut out = String::new();
    out += &match windows_table(quota, now as i64) {
        Some(t) => format!("**各订阅当前窗口**\n\n{t}\n"),
        None => "**各订阅当前窗口**：还没有收到任何订阅的窗口信息。\n\n".into(),
    };
    let total = |s: u64| {
        let mut u = Use::default();
        let n = lines.iter().filter(|v| v["t"].as_u64().unwrap_or(0) + s >= now).map(|v| u.add(&Use { model: String::new(), input: v["in"].as_u64().unwrap_or(0), output: v["out"].as_u64().unwrap_or(0), cache_read: v["cr"].as_u64().unwrap_or(0), cache_write: v["cw"].as_u64().unwrap_or(0), usd: v["usd"].as_f64() })).count();
        (n, u)
    };
    out += "**合计**：";
    out += &[5 * 3600, 86400, 7 * 86400].iter().map(|s| { let (n, u) = total(*s); format!("{} {} 条、{}、{} token", span_name(*s), n, usd(u.usd), tok(u.tokens())) }).collect::<Vec<_>>().join("；");
    out += "\n\n";
    let recent: Vec<Value> = lines.iter().filter(|v| v["t"].as_u64().unwrap_or(0) + secs >= now).cloned().collect();
    if recent.is_empty() { out += &format!("最近 {} 没有用量记录。\n", span_name(secs)); return out; }
    out += &format!("**最近 {}，按话题**\n\n{}\n", span_name(secs), table(&rank(&recent, "topic"), "话题", 15));
    out += &format!("**最近 {}，按订阅**\n\n{}\n", span_name(secs), table(&rank(&recent, "acct"), "订阅", 15));
    out += "「折算」是 agent 自报的按标价折算的美元（claude、pi 有，codex 没有），只用来比较谁更费额度；订阅不按它扣费。「轮」按每轮每个模型计。\n";
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_agent_reports() {
        let r = json!({"type": "result", "modelUsage": {"m": {"inputTokens": 999}}, "model_usage_since_last_result": {
            "claude-opus-5-5": {"inputTokens": 6, "outputTokens": 816, "cacheReadInputTokens": 602955, "cacheCreationInputTokens": 1308, "costUSD": 0.147399},
            "claude-haiku": {"inputTokens": 0, "outputTokens": 0}}});
        assert_eq!(from_claude_result(&r), vec![Use { model: "claude-opus-5-5".into(), input: 6, output: 816, cache_read: 602955, cache_write: 1308, usd: Some(0.147399) }]);
        let old = json!({"type": "result", "usage": {"input_tokens": 3, "output_tokens": 4}});
        assert_eq!(from_claude_result(&old)[0].tokens(), 7);
        let pi = json!({"provider": "relay", "model": "auto", "usage": {"input": 10, "output": 5, "cacheRead": 100, "cacheWrite": 0, "cost": {"total": 0.01}}});
        assert_eq!(from_pi(&pi), Some(Use { model: "relay/auto".into(), input: 10, output: 5, cache_read: 100, cache_write: 0, usd: Some(0.01) }));
        let cx = from_codex(&json!({"inputTokens": 1000, "cachedInputTokens": 800, "outputTokens": 50}), "gpt-6").unwrap();
        assert_eq!((cx.input, cx.cache_read, cx.output), (200, 800, 50));
        let mut turn = vec![];
        merge(&mut turn, cx.clone()); merge(&mut turn, cx);
        assert_eq!((turn.len(), turn[0].input), (1, 400));

        let info = json!({"status": "allowed", "rateLimitType": "five_hour", "unifiedWindows": {
            "five_hour": {"resetsAt": 1795159800, "status": "allowed", "utilization": 0.28},
            "seven_day": {"resetsAt": 1795482000, "status": "allowed_warning", "utilization": 0.91}}});
        let ws = claude_windows(&info);
        assert!(ws.contains(&Window { name: "5h".into(), status: "allowed".into(), used: Some(0.28), resets: Some(1795159800) }));
        assert!(ws.contains(&Window { name: "7d".into(), status: "warning".into(), used: Some(0.91), resets: Some(1795482000) }));
        let cw = codex_windows(&json!({"primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 5}, "secondary": {"usedPercent": 40, "windowDurationMins": 10080}, "rateLimitReachedType": "primary"}));
        assert_eq!(cw[0], Window { name: "5h".into(), status: "rejected".into(), used: Some(1.0), resets: Some(5) });
        assert_eq!((cw[1].name.as_str(), cw[1].status.as_str()), ("7d", "allowed"));
    }

    #[test]
    fn keeps_within_limits() {
        let l = |t: u64| format!("{{\"t\":{t},\"topic\":\"a\"}}\n");
        let s: String = [1, 2, 10, 11, 12, 13].iter().map(|t| l(*t)).collect();
        // too old
        assert_eq!(prune(&s, 10, 10_000), [10, 11, 12, 13].iter().map(|t| l(*t)).collect::<String>());
        // too big: the newest, within half the limit
        assert_eq!(prune(&s, 0, l(10).len() * 4), [12, 13].iter().map(|t| l(*t)).collect::<String>());
        assert_eq!(span(""), Some(86400));
        assert_eq!(span("5h"), Some(18000));
        assert_eq!(span("7d"), Some(604800));
        assert_eq!(span("90m"), Some(5400));
        assert_eq!(span("x"), None);
        assert_eq!(span("0h"), None);
    }

    #[test]
    fn agy_reports() {
        let w = agy_windows("Gemini Models\tWeekly Limit Remaining\t75%\t2026-11-30T07:52:57Z\nGemini Models\tFive Hour Limit Remaining\t0%\t2026-11-23T12:52:57Z\nnoise\n");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].name, "Gemini 7d");
        assert_eq!(w[0].used, Some(0.25));
        assert_eq!(w[0].resets, Some(1796025177));
        assert_eq!(w[1].status, "rejected");
        let u = from_agy_acp(&serde_json::json!({"inputTokens": 2238, "outputTokens": 10, "thoughtTokens": 8, "cachedReadTokens": 6598, "cachedWriteTokens": 0}), "").unwrap();
        assert_eq!((u.model.as_str(), u.input, u.output, u.cache_read), ("agy", 2238, 18, 6598));
    }

    #[test]
    fn summary_text() {
        let now = 100_000u64;
        let line = |t: u64, topic: &str, acct: &str, usd: Option<f64>, inp: u64| { let mut v = json!({"t": t, "topic": topic, "acct": acct, "in": inp, "out": 10, "cr": 1000, "cw": 0}); if let Some(u) = usd { v["usd"] = json!(u); } v };
        let lines = vec![line(now - 60, "GAAC", "cc3", Some(2.0), 100), line(now - 120, "刷推特", "cc6", Some(5.0), 100),
                         line(now - 30, "刷推特", "cc6", Some(1.0), 100), line(now - 90000, "旧", "cc3", Some(9.0), 100),
                         line(now - 10, "codex 话题", "cx1", None, 5000)];
        let q = json!({"cc3": {"at": now - 120, "windows": [{"name": "5h", "status": "allowed", "used": 0.28, "resets": now + 8100}, {"name": "7d", "status": "allowed", "used": 0.5, "resets": now + 86400}]},
                       "cc6": {"at": now - 60, "windows": [{"name": "5h", "status": "rejected", "used": 1.0, "resets": now + 600}]},
                       "cc1": {"at": now - 99999, "windows": [{"name": "5h", "status": "rejected", "used": 1.0, "resets": now - 5}]}});
        let s = summary(&lines, &q, now, 5 * 3600);
        // windows first, the fullest first
        let w = s.find("各订阅当前窗口").unwrap();
        assert!(w < s.find("合计").unwrap());
        assert!(s.find("| cc6 | 100% 已满，10分后重置 | — |").unwrap() < s.find("| cc3 | 28% 正常，2时15分后重置 | 50% 正常，24时0分后重置 |").unwrap(), "{s}");
        assert!(s.contains("| cc1 | 已重置 |"), "{s}");
        // by topic: dollars first, the old line left out
        let by_topic = &s[s.find("按话题").unwrap()..s.find("按订阅").unwrap()];
        assert!(by_topic.find("| 刷推特 | 2 | $6.00 |").unwrap() < by_topic.find("| GAAC | 1 | $2.00 |").unwrap(), "{by_topic}");
        assert!(by_topic.contains("| codex 话题 | 1 | — | 5k | 1k | 10 |"), "{by_topic}");
        assert!(!by_topic.contains("旧"));
        assert!(s.contains("24 小时 4 条、$8.00"), "{s}");
        assert!(summary(&[], &json!({}), now, 3600).contains("最近 1 小时 没有用量记录"));
    }
}
