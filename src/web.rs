//! The interface. Every page shows real state read from where it lives; nothing is written in by hand.
use crate::agents::{self, Kind};
use crate::{brief, crypt, daemon, store};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tiny_http::{Header, Request, Response, Server};

// ---- small helpers ----

static OFFSET: OnceLock<i64> = OnceLock::new();
fn offset() -> i64 {
    *OFFSET.get_or_init(|| {
        let z = std::process::Command::new("date").arg("+%z").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        if z.len() == 5 {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let h: i64 = z[1..3].parse().unwrap_or(0);
            let m: i64 = z[3..5].parse().unwrap_or(0);
            sign * (h * 3600 + m * 60)
        } else { 0 }
    })
}

pub fn fmt_time(t: u64) -> String {
    if t == 0 { return "—".into(); }
    let s = t as i64 + offset();
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn ago(t: u64) -> String {
    if t == 0 { return "—".into(); }
    let d = store::now().saturating_sub(t);
    if d < 60 { "刚刚".into() } else if d < 3600 { format!("{} 分钟前", d / 60) } else if d < 86400 { format!("{} 小时前", d / 3600) } else { fmt_time(t) }
}

fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) { Some(v) => { out.push(v); i += 3; } None => { out.push(b'%'); i += 1; } }
            }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn form(body: &str) -> std::collections::HashMap<String, String> {
    body.split('&').filter(|p| !p.is_empty()).map(|p| { let (k, v) = p.split_once('=').unwrap_or((p, "")); (url_decode(k), url_decode(v)) }).collect()
}

fn badge(ok: Option<bool>, text: &str) -> String {
    let cls = match ok { Some(true) => "ok", Some(false) => "bad", None => "mute" };
    format!("<span class=\"badge {cls}\">{}</span>", esc(text))
}

fn result_cell(r: &Value) -> String {
    if r.is_null() { return "<span class=\"mute\">—</span>".into(); }
    if r["running"] == true { return format!("<span class=\"badge run\">测试中</span> <span class=\"mute\">{}</span>", ago(r["time"].as_u64().unwrap_or(0))); }
    let ok = r["ok"] == true;
    format!("{} <span class=\"mute\">{}</span><div class=\"msg\">{}</div>", badge(Some(ok), if ok { "正常" } else { "失败" }), ago(r["time"].as_u64().unwrap_or(0)), esc(r["msg"].as_str().unwrap_or("")))
}

const JS: &str = r#"<script>
(function(){
  var main=document.querySelector("main");
  function swapAll(doc){main.classList.add("swap","out");setTimeout(function(){main.innerHTML=doc.querySelector("main").innerHTML;main.classList.remove("out");},180);}
  function morph(doc){
    var olds=main.querySelectorAll("[data-k]"),news=doc.querySelectorAll("main [data-k]");
    if(olds.length!==news.length){swapAll(doc);return;}
    news.forEach(function(n){var o=main.querySelector('[data-k="'+n.getAttribute("data-k")+'"]');
      if(!o){return;} if(o.innerHTML!==n.innerHTML){o.innerHTML=n.innerHTML;o.classList.remove("flash");void o.offsetWidth;o.classList.add("flash");}});
  }
  function fast(doc){return !!(doc.querySelector(".badge.run"));}
  var timer=null;
  function poll(){fetch(location.href,{cache:"no-store"}).then(function(r){return r.text();}).then(function(t){
    var doc=new DOMParser().parseFromString(t,"text/html");morph(doc);schedule(fast(doc));}).catch(function(){schedule(false);});}
  function schedule(f){clearTimeout(timer);timer=setTimeout(poll,f?2000:12000);}
  schedule(!!document.querySelector(".badge.run"));
  var words={"保存":"保存中","测试":"测试中","测试连接":"连接中","删除":"删除中","添加":"添加中","重新生成":"生成中","重新写入":"写入中","立即同步并备份":"同步中","全部测试":"测试中"};
  document.addEventListener("submit",function(e){
    var f=e.target,b=e.submitter||f.querySelector("button");
    if(f.method.toLowerCase()!=="post")return;
    e.preventDefault();
    if(b){b.classList.add("busy");b.dataset.t=b.textContent;if(words[b.textContent])b.textContent=words[b.textContent];}
    fetch(f.action,{method:"POST",body:new URLSearchParams(new FormData(f)),redirect:"follow"}).then(function(r){
      var to=new URL(r.url);return r.text().then(function(t){
        var doc=new DOMParser().parseFromString(t,"text/html");
        if(to.pathname!==location.pathname){history.pushState(null,"",to.pathname);swapAll(doc);} else {morph(doc);}
        if(b){b.classList.remove("busy");b.textContent=b.dataset.t;}
        schedule(fast(doc));});
    }).catch(function(){if(b){b.classList.remove("busy");b.textContent=b.dataset.t;}});
  });
  window.addEventListener("popstate",function(){location.reload();});
})();
</script>"#;

const CSS: &str = r#"
:root{--bg:#f6f6f4;--fg:#1d1d1b;--mute:#6b6b66;--card:#fff;--line:#e3e2dc;--accent:#2f5d50;--ok:#1f7a4d;--bad:#b3261e;--warn:#9a6700}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ecebe6;--mute:#9c9b94;--card:#1f1f1d;--line:#33332f;--accent:#8cc5b2;--ok:#5cc28f;--bad:#f2867d;--warn:#e0b44c}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.55 -apple-system,BlinkMacSystemFont,"PingFang SC","Noto Sans SC",sans-serif}
header{display:flex;flex-wrap:wrap;gap:4px 18px;align-items:center;padding:14px 20px;border-bottom:1px solid var(--line);background:var(--card)}
header b{font-size:16px;margin-right:8px}header a{color:var(--mute);text-decoration:none;padding:4px 0}header a.on{color:var(--fg);border-bottom:2px solid var(--accent)}
main{max-width:1100px;margin:0 auto;padding:20px 16px 60px}h2{font-size:15px;margin:26px 0 10px}h2:first-child{margin-top:0}
.card{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:14px 16px;margin-bottom:14px;overflow-x:auto}
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:7px 8px;border-bottom:1px solid var(--line);vertical-align:top}th{color:var(--mute);font-weight:500;font-size:12px}
tr:last-child td{border-bottom:none}.mute{color:var(--mute)}.msg{color:var(--mute);font-size:12px;max-width:420px;word-break:break-all;white-space:pre-wrap}
.badge{display:inline-block;padding:0 7px;border-radius:9px;font-size:12px;border:1px solid currentColor}.badge.ok{color:var(--ok)}.badge.bad{color:var(--bad)}.badge.mute{color:var(--mute)}.badge.warn{color:var(--warn)}
input,select,textarea{font:inherit;color:inherit;background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:5px 8px;min-width:0}
textarea{width:100%;min-height:420px;font-family:ui-monospace,Menlo,monospace;font-size:13px}
button{font:inherit;cursor:pointer;border:1px solid var(--line);background:var(--card);color:var(--fg);border-radius:6px;padding:4px 10px}button.primary{background:var(--accent);color:var(--bg);border-color:var(--accent)}button.danger{color:var(--bad)}
form.inline{display:inline}.row{display:flex;flex-wrap:wrap;gap:8px;align-items:center;margin:6px 0}.row label{color:var(--mute);font-size:12px}
pre{white-space:pre-wrap;word-break:break-word;font-family:ui-monospace,Menlo,monospace;font-size:12.5px;margin:0}code{font-family:ui-monospace,Menlo,monospace;font-size:12.5px}
@keyframes spin{to{transform:rotate(360deg)}}@keyframes pulse{50%{opacity:.35}}@keyframes fade{from{opacity:0;transform:translateY(-4px)}to{opacity:1}}button{transition:background .15s,opacity .15s,transform .05s}button:active{transform:scale(.97)}button:hover{filter:brightness(1.06)}button.busy{opacity:.7;pointer-events:none}button.busy::before{content:"";display:inline-block;width:11px;height:11px;margin-right:6px;vertical-align:-1px;border:2px solid currentColor;border-right-color:transparent;border-radius:50%;animation:spin .7s linear infinite}.badge.run{color:var(--warn);animation:pulse 1.2s ease-in-out infinite}.badge.run::before{content:"";display:inline-block;width:9px;height:9px;margin-right:5px;vertical-align:-1px;border:2px solid currentColor;border-right-color:transparent;border-radius:50%;animation:spin .8s linear infinite}main{animation:fade .2s ease-out}@keyframes flash{from{background:color-mix(in srgb,var(--accent) 22%,transparent)}to{background:transparent}}.flash{animation:flash 1.2s ease-out}.swap{transition:opacity .18s}.swap.out{opacity:0}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:8px}
"#;

fn page(active: &str, title: &str, body: &str, refresh: bool) -> String {
    let tabs = [("/", "会话"), ("/devices", "设备"), ("/memory", "记忆"), ("/messages", "消息记录"), ("/prompt", "启动说明"), ("/settings", "设置"), ("/log", "日志")];
    let nav: String = tabs.iter().map(|(h, t)| format!("<a href=\"{h}\"{}>{t}</a>", if *h == active { " class=\"on\"" } else { "" })).collect();
    let meta = if refresh { "<meta name=\"live\" content=\"fast\">" } else { "" };
    format!("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">{meta}<title>{} · exterior</title><style>{CSS}</style></head><body><header><b>exterior</b>{nav}</header><main>{body}</main>{JS}</body></html>", esc(title))
}

// ---- pages ----

fn sessions_page() -> String {
    let notes = store::notes();
    let mut rows = String::new();
    let mut any_running = false;
    for n in notes.iter().filter(|n| n["deleted"] != true) {
        let tid = n["topic"].as_str().unwrap_or("");
        let status = n["status"].as_str().unwrap_or("未启动");
        let (live, pending) = daemon::running(tid).unwrap_or((false, false));
        let shown = if status == "运行中" && !pending && live { "空闲".to_string() } else { status.to_string() };
        if pending || status == "转移中" { any_running = true; }
        let cls = match shown.as_str() { "运行中" | "转移中" => Some(true), "出错" => Some(false), _ => None };
        let running_now = matches!(shown.as_str(), "运行中" | "转移中");
        let agent = n["agent"].as_str().unwrap_or("—");
        let agent = match n["resolved"].as_str() { Some(r) if r != agent => format!("{agent} → {r}"), _ => agent.to_string() };
        let from = n["from"].as_str().map(|f| format!("<div class=\"msg\">从 {} 转来</div>", esc(f))).unwrap_or_default();
        let err = n["last_error"].as_str().map(|e| format!("<div class=\"msg\">{}</div>", esc(e))).unwrap_or_default();
        let link = format!("https://t.me/c/GROUP_ID/{}", tid.trim_start_matches("tgbot:"));
        rows.push_str(&format!(
            "<tr data-k=\"s:{}\"><td><a href=\"{link}\">{}</a><div class=\"mute\">{}</div></td><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}{}{}{}</td><td>{}</td><td class=\"msg\">{}</td></tr>",
            esc(tid), esc(n["name"].as_str().unwrap_or("")), esc(tid), esc(n["device"].as_str().unwrap_or("")), esc(&agent),
            esc(n["session"].as_str().unwrap_or("—")), if running_now { format!("<span class=\"badge run\">{}</span>", esc(&shown)) } else { badge(cls, &shown) }, if live { " <span class=\"mute\">进程在</span>" } else { "" }, from, err,
            ago(n["active"].as_u64().unwrap_or(0)), esc(n["dir"].as_str().unwrap_or(""))));
    }
    if rows.is_empty() { rows = "<tr><td colspan=7 class=\"mute\">还没有会话。在 Telegram 群里新建一个话题，发一句话。</td></tr>".into(); }
    let deleted = notes.iter().filter(|n| n["deleted"] == true).count();
    let tg = store::state("telegram");
    let body = format!(
        "<h2>会话</h2><div class=\"row\">brief：{} <span class=\"mute\">{}</span></div><div class=\"card\"><table><tr><th>话题</th><th>设备</th><th>Agent</th><th>会话编号</th><th>状态</th><th>最后活动</th><th>文件夹</th></tr>{rows}</table></div>{}",
        badge(Some(tg["ok"] == true), tg["msg"].as_str().unwrap_or("未知")), ago(tg["time"].as_u64().unwrap_or(0)),
        if deleted > 0 { format!("<p class=\"mute\">另有 {deleted} 个话题已在 Telegram 里删除，它们的文件夹和记录都还在。</p>") } else { String::new() });
    page("/", "会话", &body, any_running)
}

fn devices_page() -> String {
    let c = store::config();
    let tests = store::state("tests");
    let mut body = String::from("<h2>设备</h2>");
    let mut testing = false;
    for (i, d) in c["devices"].as_array().into_iter().flatten().enumerate() {
        let dn = d["name"].as_str().unwrap_or("");
        let conn = &tests[format!("dev:{dn}")];
        if conn["running"] == true { testing = true; }
        let has_pw = d["password"].as_str().map(|s| !s.is_empty()).unwrap_or(false);
        body.push_str(&format!(r#"<div class="card"><form method="post" action="/devices/save"><input type="hidden" name="i" value="{i}">
<div class="grid"><div><label class="mute">名称</label><br><input name="name" value="{}"></div><div><label class="mute">地址</label><br><input name="host" value="{}"></div>
<div><label class="mute">端口</label><br><input name="port" value="{}"></div><div><label class="mute">用户</label><br><input name="user" value="{}"></div>
<div><label class="mute">密码{}</label><br><input name="password" type="password" placeholder="{}" autocomplete="new-password"></div><div><label class="mute">根目录</label><br><input name="root" value="{}"></div></div>
<div class="row"><button class="primary">保存</button></form>
<form class="inline" method="post" action="/devices/test"><input type="hidden" name="i" value="{i}"><button>测试连接</button></form>
<form class="inline" method="post" action="/devices/delete" onsubmit="return confirm('删除设备「{}」？')"><input type="hidden" name="i" value="{i}"><button class="danger">删除</button></form>
<span data-k="dev:{}">{}</span></div>"#,
            esc(dn), esc(d["host"].as_str().unwrap_or("")), d["port"].as_u64().unwrap_or(22), esc(d["user"].as_str().unwrap_or("")),
            if has_pw { "（已加密保存）" } else { "" }, if has_pw { "不改就留空" } else { "" }, esc(d["root"].as_str().unwrap_or("")), esc(dn), esc(dn), result_cell(conn)));
        body.push_str(&format!("<h2>Agent{}</h2><div class=\"row\"><form class=\"inline\" method=\"post\" action=\"/agents/testall\"><input type=\"hidden\" name=\"i\" value=\"{i}\"><button>全部测试</button></form><span class=\"mute\">逐个测试这台设备上的所有 agent</span></div><table><tr><th>名称</th><th>启动命令</th><th>类型</th><th></th><th>测试结果</th></tr>", if i == 0 { "（第一行是新话题的默认 agent）" } else { "" }, i = i));
        for (j, a) in d["agents"].as_array().into_iter().flatten().enumerate() {
            let an = a["name"].as_str().unwrap_or("");
            let r = &tests[format!("agent:{dn}:{an}")];
            if r["running"] == true { testing = true; }
            let kind = a["kind"].as_str().unwrap_or("");
            let opts: String = ["claude", "codex", "pi"].iter().map(|k| format!("<option value=\"{k}\"{}>{}</option>", if *k == kind { " selected" } else { "" }, Kind::parse(k).unwrap().label())).collect();
            body.push_str(&format!(r#"<tr><form method="post" action="/agents/save"><input type="hidden" name="i" value="{i}"><input type="hidden" name="j" value="{j}">
<td><input name="name" value="{}" size="8"></td><td><input name="command" value="{}" size="22"></td><td><select name="kind">{opts}</select></td>
<td style="white-space:nowrap"><button>保存</button></form>
<form class="inline" method="post" action="/agents/test"><input type="hidden" name="i" value="{i}"><input type="hidden" name="j" value="{j}"><button>测试</button></form>
<form class="inline" method="post" action="/agents/delete"><input type="hidden" name="i" value="{i}"><input type="hidden" name="j" value="{j}"><button class="danger">删除</button></form></td>
<td data-k="a:{}:{}">{}</td></tr>"#, esc(an), esc(a["command"].as_str().unwrap_or("")), esc(dn), esc(an), result_cell(r)));
        }
        body.push_str(&format!(r#"<tr><form method="post" action="/agents/save"><input type="hidden" name="i" value="{i}"><input type="hidden" name="j" value="new">
<td><input name="name" placeholder="名称" size="8"></td><td><input name="command" placeholder="启动命令" size="22"></td><td><select name="kind"><option value="claude">Claude</option><option value="codex">Codex</option><option value="pi">Pi</option></select></td><td><button>添加</button></td><td></td></form></tr></table></div>"#));
    }
    body.push_str(r#"<h2>添加设备</h2><div class="card"><form method="post" action="/devices/save"><input type="hidden" name="i" value="new">
<div class="grid"><div><label class="mute">名称</label><br><input name="name"></div><div><label class="mute">地址</label><br><input name="host" placeholder="local 表示本机"></div>
<div><label class="mute">端口</label><br><input name="port" value="22"></div><div><label class="mute">用户</label><br><input name="user"></div>
<div><label class="mute">密码</label><br><input name="password" type="password" autocomplete="new-password"></div><div><label class="mute">根目录</label><br><input name="root"></div></div>
<div class="row"><button class="primary">添加</button></div></form></div>"#);
    page("/devices", "设备", &body, testing)
}

fn memory_page() -> String {
    let mut body = String::new();
    for d in store::devices() {
        let dn = d["name"].as_str().unwrap_or("").to_string();
        let p = crate::prompt::infra(&d);
        let tree = crate::host::run(&d, &format!("cd {} 2>/dev/null || {{ echo '没有这个目录'; exit 0; }}; find . -name '*.md' -not -path './.git/*' -not -path './archive/*' | sed 's#^./##' | sort", crate::host::quote(&p)), 30).unwrap_or_else(|e| format!("读不到：{e}"));
        let last = &store::state("infra")[&dn];
        let status = if last.is_null() { "<span class=\"mute\">exterior 还没提交过</span>".to_string() } else {
            format!("{} <span class=\"mute\">{} · {}</span> <span class=\"msg\">{}</span>", badge(Some(last["ok"] == true), if last["ok"] == true { "已推送" } else { "推送失败" }),
                ago(last["time"].as_u64().unwrap_or(0)), esc(last["topic"].as_str().unwrap_or("")), esc(last["msg"].as_str().unwrap_or("")))
        };
        let log = crate::infra::log(&d, 12);
        let mut rows = String::new();
        for l in log.lines().map(str::trim_end) {
            let first = l.split_whitespace().next().unwrap_or("");
            if !l.starts_with(' ') && first.len() >= 7 && first.chars().all(|c| c.is_ascii_hexdigit()) {
                rows.push_str(&format!("<div class=\"row\"><code>{}</code><form class=\"inline\" method=\"post\" action=\"/infra/revert\" onsubmit=\"return confirm('撤回这次修改？')\"><input type=\"hidden\" name=\"d\" value=\"{}\"><input type=\"hidden\" name=\"h\" value=\"{}\"><button>撤回</button></form></div>", esc(l), esc(&dn), esc(first)));
            } else { rows.push_str(&format!("<div class=\"msg\">{}</div>", esc(l))); }
        }
        let r = &store::state("tests")[format!("revert:{dn}")];
        body.push_str(&format!(r#"<h2>{}：{}</h2><div class="card"><div class="row mute">关于 Yao 的知识只放这一处（GitHub：BH3GEI/exterior-knowledge）。agent 用到时自己查；学到新东西直接改对应文件，exterior 每轮结束自动提交并推送。</div><div class="row">最近一次：{}</div><div class="row"><form class="inline" method="post" action="/infra/test"><input type="hidden" name="d" value="{}"><button>测试推送</button></form>{}</div></div>
<h2>最近的修改</h2><div class="card">{}<div class="row">{}</div></div><h2>文件</h2><div class="card"><pre>{}</pre></div>"#,
            esc(&dn), esc(&p), status, esc(&dn), result_cell(&store::state("tests")[format!("infra:{dn}")]), if rows.is_empty() { "<div class=\"mute\">没有记录</div>".into() } else { rows }, result_cell(r), esc(&tree)));
    }
    if body.is_empty() { body = "<p class=\"mute\">还没有设备。</p>".into(); }
    page("/memory", "记忆", &body, false)
}



fn messages_page() -> String {
    let mut rows: Vec<String> = store::tail("messages.jsonl", 200).iter().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|m| {
        format!("<tr><td class=\"mute\" style=\"white-space:nowrap\">{}</td><td>{}</td><td>{}</td><td class=\"msg\">{}</td></tr>", fmt_time(m["time"].as_u64().unwrap_or(0)), esc(m["from"].as_str().unwrap_or("")), esc(m["to"].as_str().unwrap_or("")), esc(m["text"].as_str().unwrap_or("")))
    }).collect();
    if rows.is_empty() { rows.push("<tr><td colspan=4 class=\"mute\">还没有会话之间的消息。</td></tr>".into()); }
    page("/messages", "消息记录", &format!("<h2>会话之间的消息</h2><div class=\"card\"><table><tr><th>时间</th><th>发送方</th><th>接收方</th><th>内容</th></tr>{}</table></div>", rows.join("")), false)
}

fn prompt_page() -> String {
    let mut body = String::from("<h2>启动说明</h2><p class=\"mute\">exterior 启动 agent 时，把下面这段直接交给它（claude 和 pi 用 --append-system-prompt，codex 用 developer_instructions），不往文件夹里写文件。话题名、文件夹按实际话题填。各工具自己的全局说明只剩红线和 knowledge 的位置。</p>");
    for d in store::devices() {
        let root = crate::prompt::root(&d);
        let r = &store::state("tests")[format!("prompt:{}", d["name"].as_str().unwrap_or(""))];
        let sample = crate::prompt::session_text(&d, "某话题", "tgbot:话题号", &format!("{root}/某话题"), false);
        let global = crate::prompt::session_text(&d, "全局", "tgbot:话题号", &format!("{root}/overview"), true);
        let extra = global.strip_prefix(sample.replace("某话题", "全局").replace(&format!("{root}/全局"), &format!("{root}/overview")).as_str()).unwrap_or("").trim().to_string();
        body.push_str(&format!("<h2>{}</h2><div class=\"card\"><pre>{}</pre><div class=\"row mute\">「全局」话题另加一段：</div><pre>{}</pre><div class=\"row\"><form method=\"post\" action=\"/prompt/install\"><input type=\"hidden\" name=\"d\" value=\"{}\"><button>重新写入 brief 密钥</button></form>{}</div></div>",
            esc(d["name"].as_str().unwrap_or("")), esc(&sample), esc(&extra), esc(d["name"].as_str().unwrap_or("")), result_cell(r)));
    }
    page("/prompt", "启动说明", &body, false)
}

fn settings_page() -> String {
    let c = store::config();
    let has_key = c["brief"]["key"].as_str().map(|s| !s.is_empty()).unwrap_or(false);
    let tg = store::state("telegram");
    let menu_n = store::state("menu").as_object().map(|m| m.len()).unwrap_or(0);
    let r = &store::state("tests")["menu"];
    let body = format!(r#"<h2>brief</h2><div class="card"><form method="post" action="/settings/save">
<div class="grid"><div><label class="mute">brief 地址</label><br><input name="brief_url" value="{}" size="34"></div><div><label class="mute">brief 密钥{}</label><br><input name="brief_key" type="password" placeholder="{}" autocomplete="new-password"></div></div>
<div class="row"><button class="primary">保存</button></div></form><div class="row">状态：{} <span class="mute">{}</span></div>
<div class="row">「/」命令菜单：{} 条 agent 命令，更新于 {} <form class="inline" method="post" action="/settings/menu"><button>重新生成</button></form> {}</div></div>"#,
        esc(c["brief"]["url"].as_str().unwrap_or("")), if has_key { "（已加密保存）" } else { "" }, if has_key { "不改就留空" } else { "" },
        badge(Some(tg["ok"] == true), tg["msg"].as_str().unwrap_or("未知")), ago(tg["time"].as_u64().unwrap_or(0)),
        menu_n, ago(store::state("menu_time").as_u64().unwrap_or(0)), result_cell(r));
    page("/settings", "设置", &body, r["running"] == true)
}

fn log_page() -> String {
    let lines: String = store::tail("exterior.log", 300).iter().rev().map(|l| format!("{}\n", esc(l))).collect();
    page("/log", "日志", &format!("<h2>日志（最新在上）</h2><div class=\"card\"><pre>{}</pre></div>", if lines.is_empty() { "—".into() } else { lines }), false)
}

// ---- actions ----

fn set_test(key: &str, v: Value) { let mut t = store::state("tests"); if !t.is_object() { t = json!({}); } t[key] = v; store::set_state("tests", t); }

fn background(key: String, f: impl FnOnce() -> Result<String, String> + Send + 'static) {
    set_test(&key, json!({"running": true, "time": store::now()}));
    std::thread::spawn(move || {
        let r = f();
        set_test(&key, json!({"ok": r.is_ok(), "time": store::now(), "msg": match r { Ok(s) => s, Err(e) => e }}));
    });
}

fn idx(f: &std::collections::HashMap<String, String>, k: &str) -> Option<usize> { f.get(k).and_then(|s| s.parse().ok()) }

fn act(path: &str, f: std::collections::HashMap<String, String>) -> String {
    let mut c = store::config();
    let g = |k: &str| f.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    match path {
        "/devices/save" => {
            let mut d = match idx(&f, "i") { Some(i) => c["devices"][i].clone(), None => json!({"agents": []}) };
            if d.is_null() { return "/devices".into(); }
            let old_name = d["name"].as_str().unwrap_or("").to_string();
            d["name"] = json!(g("name")); d["host"] = json!(g("host")); d["port"] = json!(g("port").parse::<u64>().unwrap_or(22));
            d["user"] = json!(g("user")); d["root"] = json!(g("root").trim_end_matches('/'));
            if !g("password").is_empty() { d["password"] = json!(crypt::seal(&g("password"))); }
            if d["name"].as_str().unwrap_or("").is_empty() { return "/devices".into(); }
            match idx(&f, "i") { Some(i) => c["devices"][i] = d.clone(), None => c["devices"].as_array_mut().unwrap().push(d.clone()) }
            store::set_config(&c);
            if !old_name.is_empty() && old_name != d["name"].as_str().unwrap_or("") {
                for n in store::notes() { if n["device"] == old_name.as_str() { store::note_set(n["topic"].as_str().unwrap_or(""), "device", d["name"].clone()); } }
            }
            "/devices".into()
        }
        "/devices/delete" => {
            if let Some(i) = idx(&f, "i") { if let Some(a) = c["devices"].as_array_mut() { if i < a.len() { a.remove(i); } } store::set_config(&c); }
            "/devices".into()
        }
        "/devices/test" => {
            if let Some(d) = idx(&f, "i").and_then(|i| c["devices"].get(i).cloned()) {
                let key = format!("dev:{}", d["name"].as_str().unwrap_or(""));
                background(key, move || {
                    let root = d["root"].as_str().unwrap_or("").to_string();
                    let out = crate::host::run(&d, &format!("uname -sn; mkdir -p {r} && test -w {r} && echo 根目录可写", r = crate::host::quote(&root)), 40)?;
                    let _ = crate::prompt::install(&d);
                    Ok(out.replace('\n', "；"))
                });
            }
            "/devices".into()
        }
        "/agents/save" => {
            let Some(i) = idx(&f, "i") else { return "/devices".into() };
            if c["devices"].get(i).is_none() || g("name").is_empty() || g("command").is_empty() { return "/devices".into(); }
            let row = json!({"name": g("name"), "command": g("command"), "kind": Kind::parse(&g("kind")).unwrap_or(Kind::Claude).name()});
            if !c["devices"][i]["agents"].is_array() { c["devices"][i]["agents"] = json!([]); }
            let l = c["devices"][i]["agents"].as_array_mut().unwrap();
            match idx(&f, "j") { Some(j) if j < l.len() => l[j] = row, Some(_) => {}, None => l.push(row) }
            store::set_config(&c);
            "/devices".into()
        }
        "/agents/delete" => {
            if let (Some(i), Some(j)) = (idx(&f, "i"), idx(&f, "j")) {
                if let Some(l) = c["devices"][i]["agents"].as_array_mut() { if j < l.len() { l.remove(j); } }
                store::set_config(&c);
            }
            "/devices".into()
        }
        "/agents/test" => {
            if let (Some(i), Some(j)) = (idx(&f, "i"), idx(&f, "j")) {
                let (d, a) = (c["devices"][i].clone(), c["devices"][i]["agents"][j].clone());
                if !a.is_null() {
                    let key = format!("agent:{}:{}", d["name"].as_str().unwrap_or(""), a["name"].as_str().unwrap_or(""));
                    background(key, move || {
                        let dir = format!("{}/overview", d["root"].as_str().unwrap_or("").trim_end_matches('/'));
                        agents::test(&d, &dir, a["command"].as_str().unwrap_or(""), Kind::parse(a["kind"].as_str().unwrap_or("")).unwrap_or(Kind::Claude))
                    });
                }
            }
            "/devices".into()
        }
        "/agents/testall" => {
            if let Some(d) = idx(&f, "i").and_then(|i| c["devices"].get(i).cloned()) {
                let dn = d["name"].as_str().unwrap_or("").to_string();
                let rows: Vec<Value> = d["agents"].as_array().cloned().unwrap_or_default();
                for a in &rows { set_test(&format!("agent:{dn}:{}", a["name"].as_str().unwrap_or("")), json!({"running": true, "time": store::now()})); }
                std::thread::spawn(move || {
                    let dir = format!("{}/overview", d["root"].as_str().unwrap_or("").trim_end_matches('/'));
                    for a in rows {
                        let key = format!("agent:{dn}:{}", a["name"].as_str().unwrap_or(""));
                        let r = agents::test(&d, &dir, a["command"].as_str().unwrap_or(""), Kind::parse(a["kind"].as_str().unwrap_or("")).unwrap_or(Kind::Claude));
                        set_test(&key, json!({"ok": r.is_ok(), "time": store::now(), "msg": match r { Ok(s) => s, Err(e) => e }}));
                    }
                });
            }
            "/devices".into()
        }
        "/infra/test" => {
            if let Some(d) = store::device(&g("d")) { background(format!("infra:{}", g("d")), move || crate::infra::test(&d)); }
            "/memory".into()
        }
        "/infra/revert" => {
            if let Some(d) = store::device(&g("d")) {
                let h = g("h");
                background(format!("revert:{}", g("d")), move || crate::infra::revert(&d, &h));
            }
            "/memory".into()
        }
        "/prompt/install" => {
            if let Some(d) = store::device(&g("d")) {
                background(format!("prompt:{}", g("d")), move || crate::prompt::install(&d).map(|_| "已写入".to_string()));
            }
            "/prompt".into()
        }
        "/settings/save" => {
            if f.contains_key("brief_url") {
                c["brief"]["url"] = json!(g("brief_url").trim_end_matches('/'));
                if !g("brief_key").is_empty() { c["brief"]["key"] = json!(crypt::seal(&g("brief_key"))); }
            }
            store::set_config(&c);
            "/settings".into()
        }
        "/settings/menu" => { background("menu".into(), || daemon::build_menu().map(|n| format!("已设置 {n} 条命令"))); "/settings".into() }
        _ => "/".into(),
    }
}

fn respond(mut req: Request) {
    let url = req.url().to_string();
    let (path, _query) = url.split_once('?').unwrap_or((&url, ""));
    let path = path.to_string();
    let html = Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
    if req.method() == &tiny_http::Method::Post {
        let mut body = String::new();
        let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
        let to = act(&path, form(&body));
        let _ = req.respond(Response::empty(303).with_header(Header::from_bytes("Location", to.as_bytes()).unwrap()));
        return;
    }
    let out = match path.as_str() {
        "/" => sessions_page(),
        "/devices" => devices_page(),
        "/memory" => memory_page(),
        "/messages" => messages_page(),
        "/prompt" => prompt_page(),
        "/settings" => settings_page(),
        "/log" => log_page(),
        "/healthz" => { let _ = req.respond(Response::from_string("ok")); return; }
        _ => { let _ = req.respond(Response::from_string("没有这个页面").with_status_code(404)); return; }
    };
    let _ = req.respond(Response::from_string(out).with_header(html));
}

pub fn serve(port: u16) {
    let server = Server::http(("0.0.0.0", port)).expect("listen");
    store::log(&format!("interface on :{port}"));
    for req in server.incoming_requests() {
        std::thread::spawn(move || respond(req));
    }
}

#[allow(dead_code)]
pub fn brief_ok() -> bool { brief::configured() }
