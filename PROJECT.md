# WASM Agent OS — 架構設計與 TODO(v2,定案 scope)

一個以 wasmtime 為 kernel、單一資料夾為 agent 全世界的微型 Agent OS。
**Scope 定案**:單一 agent/RAG 記憶(host-side SQLite)/Gateway 含 Web 前端/WASM 插件與程式碼執行延後。

核心理念:**沙箱內完整自主,沙箱邊界極簡**。安全邊界 = wasmtime Store 邊界。

---

## 1. 設計目標與非目標

**目標**
- Agent 在自己的資料夾內完整自主(讀寫、自我組織記憶)
- Host 攻擊面最小:無 socket、無 env 繼承、無 host 檔案系統可見性
- Agent 狀態 100% 在資料夾 + 一顆 DB → 可備份、可 git、可遷移、crash-safe
- LLM / embedding API key 永不進沙箱
- RAG 記憶由 agent 每日自主維護
- Gateway 提供 Web 前端:對話、觀察、核准外部資料授權

**非目標(延後)**
- WASM 插件系統、Python/程式碼執行
- 多 agent
- Prompt injection 內容掃描(靠窄邊界,不靠過濾)

---

## 2. 高層架構

**白話版**(使用者原話):`LLM` 是腦,`wasmtime` 是頭骨(容器,每次喚醒 fresh instantiate,`http_get`/`search_web` 是眼睛(唯讀,看得到外面但摸不到),`ssh_exec` 是身體(可選,唯一真的能改變外部世界的能力),`db_exec` 是海馬迴——RAG 引擎,自動運作,LLM 完全碰不到,只有 `daily_maintenance` 這個專屬蒸餾流程能把東西寫進長期記憶(`memory_get`/`memory_set` 那種隨手小抄設計試過,實際查 DB 發現從沒被用過,已拿掉)。曾經試過把每次喚醒改成真的獨立 child process,讓 Stop 按鈕能真的 `SIGKILL` 一次失控的 run——後來評估這個保證換來的部署風險(多一個要跟著部署的 binary、兩個 binary 版本可能兜不上)不值得,退回同 process 內 fresh Store 這個原本的簡單做法(見 §5)。

```
┌──────────────────────────────────────────────────────────┐
│ Host (Rust binary, "kernel")                              │
│                                                            │
│  ┌─────────────────┐   ┌───────────────────────────────┐ │
│  │ Gateway (axum)   │   │ Syscall Layer(generic ABI,     │ │
│  │  webui API・SSE  │   │ 見 §3——LLM 只能碰它自己吐的     │ │
│  │  grant 審核      │   │ JSON action 裡列出的那些)       │ │
│  │  (tofu domain)   │   │  llm_call(永遠 streaming)・embed│ │
│  └────────┬─────────┘   │  db_exec(RAG 引擎,只有         │ │
│  ┌────────▼─────────┐   │   agent/src/memory.rs 呼叫,    │ │
│  │ Scheduler         │   │   LLM 沒有 action 能碰到)       │ │
│  │ (30s tick:        │   │  http_get(GET-only,含 cache)・ │ │
│  │  cron・daily_     │   │   search_web                    │ │
│  │  maintenance・    │   │  ssh_exec(可選,唯一無審核的    │ │
│  │  scheduled_task)  │   │   寫入能力,見 §4.9)            │ │
│  └────────┬──────────┘   │  notify / chat_send            │ │
│           │ spawn_blocking│  sleep_until / schedule_task 家族│ │
│           │ instantiate  └──────────────┬──────────────────┘ │
│  ┌────────▼──────────────────────────────▼───────────────┐ │
│  │ wasmtime Store(每次喚醒 fresh instantiate,零殘留)      │ │
│  │  fuel/epoch limit・memory cap・empty env・stdio→log      │ │
│  │  ┌────────────────────────────────────────────────┐    │ │
│  │  │ agent.wasm(guest,唯一能發 syscall 的程式碼)      │    │ │
│  │  └────────────────────────────────────────────────┘    │ │
│  └────────────────────┬─────────────────────────────────────┘ │
│                       │ WASI preopen                         │
│  ┌────────────────────▼─────────────────────────────────┐    │
│  │ agent-home/ ← agent 看到的整個 "/"                      │    │
│  │  config.toml・SOUL.md・memory/(notes・skills・index.db)│    │
│  │  workspace/・scheduler/・logs/(chat_sessions/ 每個對話  │    │
│  │  來源各一份,見 §4.5/Discord adapter)                   │    │
│  └───────────────────────────────────────────────────────┘    │
└───────────────────────────┬─────────────────────────────────┘
              ┌─────────────┼──────────────┬─────────────────┐
              │ HTTPS(key)  │ HTTPS(GET-only)│ SSH(可選,唯一  │
              ▼             ▼                │  無審核寫入)   │
        LLM / Embed API   開放網路(唯讀)       ▼
                                          Docker/VM(「身體」,
                                          見 §4.9;固定目標,
                                          agent 選不了要連哪)
```

沒有 inbox/outbox——這是舊設計文字,從沒真的蓋過(`message` trigger 由 `/api/message` 同步處理,見 §4.5)。

---

## 3. Syscalls(現況:12 個實作 + 1 個未做)

| syscall | 簽名(概念) | 說明 |
|---|---|---|
| `llm_call` | `(messages[]) -> {text, usage}` | host 持 key,provider 無關(anthropic/openai/ollama 正規化);**永遠走 streaming**(每個 provider 各自的 SSE/NDJSON parser,即時把 reasoning/thinking delta 寫進 `thinking-live.txt`,並且逐 chunk 檢查 abort flag);**每日預算硬上限**(`daily_token_cap`);重試 5 次指數 backoff(所有錯誤都重試,見 §4.10);連續 3 次整次呼叫失敗會跳 circuit breaker,冷卻 60s;完整 prompt/response 落 transcript log |
| `embed` | `(texts[]) -> {vectors[], model}` | host 打 embedding API;**獨立於 llm_call 的每日預算**(`embed_daily_token_cap`,分開算,不搶同一包額度) |
| `db_exec` | `(sql, params) -> rows` | RAG 引擎,只有 `agent/src/memory.rs` 呼叫(hybrid_search/reindex),**LLM 沒有對應 action,完全碰不到**。原生 SQLite 開 `memory/index.db`(WAL mode,讓外部工具如 DB Browser 用唯讀/讀寫模式開著時不會跟 agent run 搶 lock),查詢逾時可調(預設 10s)。PRAGMA allowlist 曾漏放 `data_version`(FTS5 內部每次寫入 virtual table 都會查這個 pragma)——導致任何超過一段標題的筆記永遠只有第一段被索引,已修 |
| `sleep_until` | `(timestamp)` | agent 宣告下次喚醒,結束本次執行 |
| `notify` | `(message)` | 單向通知人類,寫 `logs/notifications.jsonl`,gateway Live Log 面板顯示;不會被人類看到當作聊天回覆 |
| `chat_send` | `(message, target?)` | 主動推播一則訊息到真實聊天介面(webui 或 Discord),給背景喚醒(cron/daily_maintenance/scheduled_task)用,不是回覆用(那是 `done.summary`) |
| `http_get` | `(url) -> {status, body, total_bytes, cache_path}` | **GET-only**,request/action schema 裡沒有 `method` 欄位(結構性拿掉 POST,見 §4.6/§4.10)。Denylist 私網段防 SSRF;全量 egress log;`network.get_mode` 控管(open/tofu/allowlist);secret 佔位符注入(§4.8)。`body` 截斷在 `response_max_bytes`(預設 100,000 bytes),但完整內容另存 `workspace/.http_cache/<url-hash>.txt`(`cache_path` 回傳這個路徑),agent 可用 `read_file` 分頁讀過截斷點;cache 有 TTL 過期(`http_cache_ttl_secs`,預設 24h)+ LRU 依 mtime 驅逐(`http_cache_max_bytes`,預設 20MB) |
| `search_web` | `(query) -> {results[]}` | tavily 或 self-host searxng,一般網頁搜尋 |
| `ssh_exec` | `(command) -> {stdout, stderr, exit_code, timed_out}` | 見 §4.9——固定目標、無 pty、硬 wall-clock deadline、全量 audit log,是專案裡**唯一沒有事前審核的寫入能力** |
| `schedule_task` / `update_task` / `delete_task` | CRUD | agent 自己設「每天早上提醒我」這種排程任務(`scheduler/<id>.json`),不用人手動去前端建;webui 也能直接編輯 task 的 `data_path` 內容(`GET/PUT /api/scheduler/task_file`) |
| `request_external` | **尚未實作** | 原設計:單次讀外部資料掛起 → gateway 審核 → 複製進 `inbox/granted/`。使用者明確決定先不做——`http_get` 自己的 grant queue(tofu 新 domain 審核)已經覆蓋大部分「網路動作要人審」的需求,這條專用管道暫緩;呼叫會回 `"request_external not implemented yet"` |

**`memory_get`/`memory_set`(已移除)**:曾經是單一事實記憶的 flat `kv` 表存取(取代原始 SQL action `db_query`),後來直接查 DB 發現 `kv` 表從沒被真的用過(0 rows)——拿掉,連同 prompt 文件裡的說明一起刪。

**`db_exec` 加固(必做,SQL 在 host 執行)**
- 關閉 `load_extension`
- `sqlite3_set_authorizer` 禁 `ATTACH` / `DETACH`
- 限制危險 `PRAGMA`
- 只允許開啟 agent-home 內的那顆 db 檔

**ABI 是通用的,不是每個 syscall 各自的安全邊界**:`env::syscall(name, req)` 一個 host import,guest 端(agent 自己的 Rust code)理論上什麼名字都能叫。真正決定「LLM 能不能觸發某個 syscall」的是 `agent_loop.rs` 的 action match——只有出現在那個 match 裡的字串,LLM 吐出來的 JSON 才叫得動;`db_exec` 沒有對應 action,所以永遠只能被 `memory.rs` 自己的 Rust code(RAG 自動流程)呼叫,不是 LLM 決定的。

---

## 4. 元件設計

### 4.1 Kernel
- wasmtime embed,每次喚醒 fresh instantiate(零殘留、crash-safe)
- 資源限制:epoch interruption 逾時(5 min)、memory cap(512MB)
- WASI:`preopen(agent-home, "/")`、env 空、stdio → `logs/`、給 clock/random

### 4.2 Agent(guest .wasm,Rust → wasm32-wasip1)
- 入口 `run(trigger_json)`;trigger 現況五種:`message`(webui/Discord 即時訊息,帶 `history`)/ `cron`(agent 自己 `sleep_until` 要求的時間到)/ `daily_maintenance` / `scheduled_task`(使用者或 agent 自建的排程)/ `compact_session`(session compact 專用,summarize-then-replace)。沒有 `inbox`/`grant` 這兩種 trigger,舊設計文字,從沒真的做
- Loop:讀狀態 → RAG 檢索(每 turn 都重新做,不是只在開頭,見 §4.3)→ 組 prompt → llm_call → 解析行動 → 執行 → 寫回 → sleep_until。每輪 llm_call 回來的 `input_tokens` 超過 `runtime.in_run_compact_tokens`(預設 150,000)會觸發 run 內自己的 compact——system prompt + 最開頭那則任務訊息永遠保留(避免摘要層層疊加後忘記最初的意圖),中間全部摘要掉,跟 `daily_maintenance`(記憶蒸餾)、`[chat] auto_compact_tokens`(跨 run 的 session 壓縮)是三件不同的事
- 行動格式(tool-use JSON):`read_file`(可選 `start_line`/`head_lines`/`tail_lines`/`byte_offset`/`byte_count` 分頁,超過 100,000 bytes 沒給參數就自動只給前 200 行)/ `grep_file`(純字串比對找行號)/ `write_file` / `append_file` / `list_dir` / `make_dir` / `delete_path` / `notify` / `chat_send` / `http_get` / `search_web` / `ssh_exec` / `use_skill` / `save_skill` / `schedule_task` / `update_task` / `delete_task` / `request_external` / `done`
  - **`db_query`(原始 SQL action)已拿掉**——`db_exec` syscall 本體沒拿掉,RAG 的 `chunks`/`chunks_fts` 全靠它(`agent/src/memory.rs`),只是不開放給 LLM 自己下 SQL
  - **`memory_get`/`memory_set`(曾經取代 `db_query` 的單一事實記憶 action)已拿掉**——直接查 DB 發現對應的 `kv` 表從沒被寫過一筆,拿掉連同 prompt 文件說明一起刪
  - **`write_file`/`append_file` 現在依 trigger type 限定寫入範圍**:`message`(即時聊天)只能寫 `/workspace/`,寫別的路徑直接被拒絕(附錯誤訊息說明這輪活動已經自動進 log.md,等下次 `daily_maintenance` 蒸餾);背景喚醒(`cron`/`daily_maintenance`/`scheduled_task`/`manual`)維持完全存取,因為 scheduled task 本來就有合法理由要維護自己在 `memory/notes/` 底下的狀態檔,`daily_maintenance` 更是唯一設計上「該」寫 curated notes 的地方。動機:聊天途中隨口一句話就能直接改寫 curated 筆記,沒有蒸餾/合併步驟,已經真的發生過「一個已修正的事實被不相關的後續對話又寫壞一次」的事故
- **`/SOUL.md`**(仿 OpenClaw 的 persona 概念):跟 `config.toml` 一樣是 agent-home 根目錄下一個檔案,純自由格式 markdown(persona/價值觀/語氣),每次組 system prompt 都全文帶入(不像 skills 用 progressive disclosure)。沒有專屬 action——`read_file` 隨時可讀,但改動走人類這邊(gateway `/api/soul`,GET/POST,跟 `/api/config` 同款,純文字直通無 schema,+ 前端 Soul 分頁),即時聊天的 `write_file` 已經限定在 `/workspace/`,碰不到 `/SOUL.md` 了。不存在時視為空,不影響運作。

### 4.3 記憶子系統(agent 自維護 RAG)
```
memory/
  notes/                        # LLM 蒸餾的記憶筆記(markdown,人可讀可改)
  notes/<date>/log.md           # 每次 run 的原始日誌,絕不進索引(見下)
  index.db                      # SQLite(WAL mode):chunks + FTS5(BM25)+ 向量
  maintenance_reports/
    <date>_<HHMM>.md            # 每次 daily_maintenance 一份報告
    .last_run                   # 上次成功 run 的 unix timestamp(since_ts 檢查點)
```
- **讀路徑**:hybrid 檢索——FTS5 BM25 + 向量 top-N → RRF → top-k 進 prompt。**每個 turn 都重新做**,不是只在 `run()` 開頭:第 0 turn 用開場那次(trigger 原文當 query),第 1 turn 起改用「剛剛那個 tool result 的內容」當 query 重新檢索,結果附加到那個 tool result 訊息尾巴(不新增訊息,避免破壞 user/assistant 嚴格交替)。長對話(例如多輪 `ssh_exec` 探索)話題跑掉時,記憶會跟著更新,不會整個 run 都用開場那批舊結果。只有 `memory/notes/*.md`(頂層檔案)會被索引——`notes/<date>/log.md` 這種日誌子目錄故意排除,曾經全部索引過,結果一個已經修正的舊事實因為被逐字引用在某次對話裡,永久卡在檢索結果裡
- **寫路徑,現在是 6 小時循環的 `daily_maintenance`(不是每日一次)**:
  1. 只看自己上次成功執行之後新增的 log(`since_ts`,存在 `.last_run`)——run 失敗(`run aborted: ...`)不會推進這個 checkpoint,單次失敗不會讓一整個時間窗被跳過沒審到
  2. 蒸餾新增內容 → `notes/`(這是**唯一**允許直接 `write_file`/`append_file` 進 curated notes 的 trigger type,見 §4.2)
  3. content hash 增量:只 re-chunk + re-embed 變動檔
  4. LLM 自主整理:合併重複、過期降級為摘要、修剪 log
  5. 產出 `maintenance_reports/<date>_<HHMM>.md`(gateway 可看,一天可能不只一份)
- Schema:`chunks(source_path, content_hash, text, embedding BLOB, embed_model)`;`embed_model` 不符 → 自動全庫重嵌。**曾經有個真的發生過的 bug**:`db_exec` 的 authorizer PRAGMA allowlist 漏放 `data_version`(FTS5 每次寫入 virtual table 內部都會查這個),導致任何超過一段標題的筆記,`chunks_fts` 那半只有第一段插得進去,其餘靜默失敗——已修
- 向量檢索先用 host 端 sqlite-vec(原生編譯無壓力);筆數少時 BLOB + 暴力 cosine 也行

### 4.4 Gateway(axum + Web 前端)
Kernel space;與 agent 的接觸面僅為檔案系統 + 喚醒,agent 不知其存在。

**API(現況,`kernel/src/gateway.rs` 實際 route 清單)**——`pause`/`resume`/`reload` 這幾個舊設計文字**明確決定不做**(見下方 4.5/`request_external` 段落),沒有對應 endpoint

| Endpoint | 功能 |
|---|---|
| `POST /api/message` | webui 聊天訊息,同步等整個 run 跑完(可能好幾分鐘)才回,`handle_chat_message` 負責 session 讀寫 |
| `POST /api/wake` | 手動立即喚醒(開發調試用) |
| `GET /api/status` | budget 用量、`last_run`、**`busy`**(`AppState.active_runs` 這個 atomic counter > 0,不分 trigger type、不分 session,只代表「現在有東西在跑」) |
| `GET /api/session`・`POST /api/session/reset`・`POST /api/session/compact` | webui session 讀取/重置/壓縮(archive-first,不直接丟資料) |
| `GET /api/thinking`(SSE)・`GET /api/thinking/snapshot` | 即時 trace 串流 + 一次性讀檔(snapshot 用來繞過 SSE poll tick 錯過的race) |
| `POST /api/abort` | 中斷正在跑的這一次——寫 abort flag,`llm_call` 逐 chunk 檢查,剛好在串流中的那次能乾淨收尾。**純 cooperative,不保證瞬間停**:卡在 `http_get`/`ssh_exec`,或 `llm_call` 還沒收到第一個 byte,這個 flag 完全攔不住,run 就是跑到完。曾經做過真的 `SIGKILL` 子行程那套,後來評估部署風險太高,退回這個簡單版本(見 §5) |
| `GET /api/logs`(SSE) | notifications.jsonl 即時串流 |
| `GET /api/grants`・`POST /api/grants/{id}/approve\|deny` | `tofu_domain` 新網域審核(`http_write` 那條路已拿掉,見 §4.6) |
| `GET /api/egress`・`GET /api/llm/logs` | 全量 http_get egress log、llm_call transcript log,兩者都帶 `source`(哪個 channel/session 觸發的,見 §4.10) |
| `GET /api/memory/notes`・`GET /api/memory/reports` | 記憶筆記與維護報告瀏覽 |
| `GET /api/config`・`POST /api/config` | config.toml 讀/寫(寫入前驗證能 parse) |
| `GET /api/soul`・`POST /api/soul` | `/SOUL.md` 讀/寫,純文字直通 |
| `POST /api/secrets` | 寫入 vault(write-only,沒有對應 GET) |
| `GET /api/skills`・`POST /api/skills`・`DELETE /api/skills/{name}` | skill CRUD,含 `used_count`/`last_used`/`created_at` 使用量統計 |
| `GET /api/scheduler/tasks`・`POST /api/scheduler/tasks`・`PUT/DELETE /api/scheduler/tasks/{id}`・`GET /api/scheduler/runs` | 排程任務 CRUD + 執行紀錄 |
| `GET /api/scheduler/task_file`・`PUT /api/scheduler/task_file` | 直接讀寫某個 task 的 `data_path` 內容(query string 帶 guest-absolute path),讓前端 Scheduler 面板選中 task 時能連內容一起看/改,不用另外去 Notes 面板找 |
| `GET /api/discord/pairing` | Discord 配對狀態/當前 rotating code |

**前端**
- Chat(session 化,含 trace 顯示、Reset/Compact)、Status、Scheduler + 執行紀錄、Notes、Soul、Skills(含使用量統計)、Grants 待審、Reports、Config、Secrets、Live Log(SSE)、Egress log、LLM logs、Apps(Discord 配對狀態,未來多 app 共用這個分頁)
- **技術決策翻案(原玩具原則:單檔嵌進 binary,不上 build 系統)**:改成真正前後端分離——`webui/` 是獨立 Vite + Vue SFC 專案(每個分頁一個 component),`vite build` 產出 `webui/dist/`。取捨:換來元件粒度更細、`npm run dev` 熱重載開發體驗;代價是多一個 Node.js 工具鏈依賴、部署變成兩個產物而非單一 binary、多一道 build 步驟。使用者明確要求此翻案。
- **kernel 完全不知道 webui 存在**:一度讓 kernel 用 `tower-http ServeDir` 動態讀 `webui/dist/`,使用者要求連這點耦合都拔掉——kernel 現在純 API server(`/api/*`,`/` 回 404),不 import 任何跟 webui 相關的路徑/概念。兩者怎麼一起跑由 `cli/`(`ebinactl`)這個獨立 CLI wrapper 負責(`agent run` 同時起 gateway + 反向代理 webui 靜態檔在同一個 port),kernel 本身完全不管——已經做完,不再是「尚未做」。
- 認證:單一 token(secrets.toml 的 `gateway_token`),避免區網亂打

### 4.5 Scheduler
- [x] 單 agent 簡化:tokio loop(`gateway.rs` `scheduler_loop`,每 30s tick 一次),管 next-wake(agent 自己 `sleep_until` 要求的時間到了 → `cron` trigger)+ `daily_maintenance`(**6 小時循環,不是每日一次**——只審自己上次成功執行之後新增的 log,`since_ts` 存在 `memory/maintenance_reports/.last_run`;run 失敗不推進這個 checkpoint,15 分鐘重試一次直到成功)。inbox watcher 不需要——沒有非同步 inbox 概念,`message` trigger 由 `/api/message` 同步處理
- 兩種自發喚醒(`daily_maintenance`/`cron`)都不帶 chat `history`,結構上就是全新 session(`agent_loop.rs` 只有拿到 `trigger.history` 才會接續對話)——不會污染正在進行的聊天 session,已用真跑驗證(scheduled run 跑完後 `logs/session.json` 完全沒被動到)
- 每次自發喚醒都留紀錄:`logs/scheduled_runs/<ts>-<type>.json`(trigger + outcome),`GET /api/scheduler/runs` 可查,前端 SchedulerPanel 列表顯示
- [x] **使用者自訂 scheduled task**(人或 agent 都能新增):格式就是「cron 時間 + 事件資料位置」——`scheduler/<id>.json`(一檔一 task,跟 `memory/skills/<name>.md` 同款,agent 也能直接 read_file/write_file),欄位 `{id, cron, data_path, description, enabled, created_at, last_run}`
  - `cron`:自寫 5 欄位 matcher(`kernel/src/cron.rs`,`*`/數字/逗號列表/`*/step`,UTC,無時區),沒拉 `cron` crate——跟 `logs::today_utc` 同一套 Howard Hinnant 手刻日期算法,不引入 chrono
  - `scheduler_loop` 每 tick 檢查所有 enabled task,cron 對上當下分鐘就發 `scheduled_task` trigger(帶 `task_id`+`data_path`,一樣不帶 history,全新 session);同一分鐘只發一次(`last_run` 同分鐘就跳過),避免 30s tick 在同一分鐘內重複觸發
  - CRUD 兩條路都通:前端 SchedulerPanel(新增/編輯/刪除/enable 開關,`GET/POST /api/scheduler/tasks`、`PUT/DELETE /api/scheduler/tasks/{id}`);聊天對話裡 agent 自己用 `schedule_task`/`update_task`/`delete_task` action(host 端 `kernel/src/syscalls/schedule_task.rs`)——agent 自己能設「每天早上提醒我」這種任務,不用人手動去前端建
  - 已用真跑驗證:建一個 `* * * * *` 的 task 指向自寫的 `/workspace/tasks/ping.md`,scheduler tick 真的抓到、真的呼叫 Kimi 跑完、`last_run` 更新;PUT 停用、DELETE 刪除、bad cron(欄位數不對)被拒絕,四條路都測過
- [x] 同一個 session 收到新訊息 → 排隊,本次跑完再喚醒;**不同 session 或背景喚醒完全並行,不排隊**:`AppState.session_locks`(每個 session_key 一把獨立 `tokio::sync::Mutex<()>`)只鎖 `message` 這個 trigger type,理由是同一個 session 的 `session.json` 是 read-modify-write,兩輪同時跑會互相蓋掉;`cron`/`daily_maintenance`/`scheduled_task`/`manual` 沒有 session 概念,完全不鎖。這是後來從「全域一把鎖」改的——曾經全部共用一個 `run_lock`,後來發現沒必要,`write_file`/`append_file` 現在依 trigger type 限定範圍(§4.2),不同 trigger type 已經不會寫到同一個目錄,鎖範圍縮到 session 夠用。剩下的殘留風險(兩個背景 run 同時 reindex `memory/index.db`、同時 git commit 撞 `.git/index.lock`)接受不解決,兩種都是 fail soft(多一筆重複 index row、少一次 commit 被下次補上),不會壞資料

### 4.6 網路政策(egress 控制模組化,config 可調)
威脅模型不是「agent 是壞人」,是「agent 被網頁內容騙」(prompt injection → exfiltration)與 SSRF。

**固定護欄(不可關)**
1. Denylist 私有網段:localhost、RFC1918、link-local、169.254.169.254(DNS 解析後查 IP 落點,防 rebinding)
2. 全量 egress log:記**完整 URL** + bytes,gateway 可查
3. ~~POST/上傳審核佇列~~——**已拿掉**(見下方現況更新)

**GET egress 模式(config 切換,預設 `open`)**
```toml
[network]
get_mode = "open"        # open | tofu | allowlist
url_max_len = 2048        # 防 query string 夾帶大量資料
daily_request_cap = 500
response_max_bytes = 100000       # http_get 回傳 body 上限(≈25-30k tokens);曾經完全沒上限,一個 400KB+ 的 raw HTML 頁面單獨就炸過一次 llm_call 的 context
http_cache_ttl_secs = 86400        # 完整頁面快取存活時間(workspace/.http_cache/),lazy sweep,每次 http_get 開頭順便清
http_cache_max_bytes = 20971520    # 快取總量上限(20MB),超過依 mtime LRU 驅逐——TTL 只擋「隨時間增長」,擋不住同個 TTL 視窗內連抓很多不同頁面

[ratelimit]               # token bucket,host 端——**每個 run 自己一份,不是真的全域**(見下)
llm_per_min = 10
http_per_min = 30
http_per_domain_per_min = 10   # 對外禮貌,防同站連打被 ban IP
```

**撞限語義(簡單版)**:block 等待最多 **3s**,bucket 補上就過;仍無配額回 `rate_limited` + retry_after。3s 相對 5 min epoch 配額可忽略,**不需 epoch 補償**。速率持續打頂 → notify(失控迴圈的早期警報)。

**現況更新(per-session 並行後)**:`[ratelimit]` 這幾個數字曾經一度變成「名字叫全域,實際每個 run 各自一份」——`TokenBucket` 在 `AgentState::new` 現造,只活在記憶體,不落地、不跨 run 共享,以前靠唯一的全域 `run_lock` 意外撐住「全域」這個語意,run 改成 per-session 並行後(見 §4.5)這個語意就破了。**已修**:`ratelimit.rs` 加了 `GlobalRateLimiters`(`OnceLock<Mutex<...>>` process-wide singleton),`AgentState` 不再自己持有 bucket,三個消耗 rate limit 的 syscall(`llm_call`/`embed`/`http_get`)都改成向這個全域 singleton 拿 token。鎖的粒度只包在單次「試拿一個 token」這個非阻塞動作上(`try_take`),等待补充的 3 秒重試迴圈在鎖外面,不會讓一個等待中的 caller 卡住其他並行 run——不然又會變成新的全域瓶頸,違背 per-session 並行本來要達到的效果。
- `open`:GET 自由(預設,好用優先)
- `tofu`:新 domain 首次使用需 gateway 核准,之後永久放行——想收資料必用新 domain,必撞審核
- `allowlist`:僅名單內 domain(最嚴)

已知取捨:`open` 模式下 GET query string 是 exfiltration 通道,url_max_len + 完整 URL log 為緩解;在意時切 `tofu`。

**[x] 現況更新:`http_fetch` 拿掉 POST/寫入,改名 `http_get`**——使用者判斷:`ssh_exec`(4.9)已經是沒有審核的寫入能力,`http_fetch` 的 POST 審核(`grants.rs` `http_write` grant)擋不住真的想繞過的情境(改叫 `ssh_exec` 跑 `curl -X POST` 就好),留著只剩摩擦、沒留下對應的防線。拿掉後使用者再加碼:不只是拒絕非 GET,乾脆改名 `http_get`、request/action schema 裡**完全沒有 `method` 欄位**——不是 runtime 擋掉,是結構上就不存在,傳了也沒用(直接忽略,還是做 GET)。`grants.rs` 刪掉 `take_approved_write`,`PendingGrant.kind` 現在只剩 `"tofu_domain"` 會被建立(舊的 `"http_write"` 記錄可能還留在 `logs/grants.json` 裡,不影響)。`tofu_domain`(GET 模式用)完全不動。

### 4.7 工具自動安裝(agent 自建 toolbox)——**已嘗試,已回退**
「安裝」= 把 .wasm 放進 workspace 外的 `tools/`。二進位信任問題由 Store 解決:惡意工具最壞搞亂 workspace,無網路、出不了 agent-home。

**流程(設計,未實作)**:agent 判斷需求 → http_fetch 搜尋/下載(GET)→ kernel 輕量驗證(合法 wasm module、size cap)→ 落地 `tools/` + 寫 `lockfile.json`(name/source_url/sha256/installed_at)→ `exec_wasm` 使用。

- **Lockfile**:gateway 可審計裝了什麼;agent 遷移時 kernel 照 lockfile 還原工具;hash 可比對官方 release
- **工具輸出視為不可信輸入**(stdout 進 LLM context = injection 通道,agent prompt 中註明)
- **磁碟配額**:agent-home 總量上限(如 2GB),超過拒寫 + notify
- 現成生態:uutils(coreutils)、jq、ripgrep、wasm-git 等皆有 WASI build
- **curl 類無意義**:網路是能力不是程式,exec_wasm 的 Store 無 socket,裝了也發不出封包——網路永遠只能經 kernel 的 http_fetch

**[~] 實際做過又回退的紀錄**:為了讓 agent 能列目錄/刪檔案,一度把 `exec_wasm` syscall 真正接上(`agent_loop.rs` 補 `Some("exec_wasm")` arm)、手刻 `tools/unix`(wasm32-wasip2 component,`ls`/`cat`/`mkdir`/`rm`)、`wasm.toml` manifest 動態組進 system prompt、`tools/` 獨立於 `workspace/` 之外的目錄分離(修掉「工具能刪自己」的洞)——全部真的建好、測過、真跑驗證過。

事後反省砍掉重做:`exec_wasm`/wasmtime component hosting 這整套複雜度存在的唯一理由是「要跑不信任的程式碼」,但「agent 自己上網抓現成工具」這個場景本身已經評估放棄(抓來源不明、且多半是 wasip2 跟舊 wasip1 沙盒不相容)——放棄之後,`tools/unix` 是我們自己寫、自己信任的程式碼,根本不需要沙盒。更進一步發現 `read_file`/`write_file` 本來就不是 host syscall,是 `agent_loop.rs` 直接用 guest 自己的 `std::fs`(agent.wasm 本來就 preopen 整個 agent_home)——照這個先例,`list_dir`/`make_dir`/`delete_path` 一樣直接加 3 個 action arm 用 guest-side `std::fs` 就好,不用多繞 wasmtime-in-wasmtime 這層。

**最終方案**:`exec_wasm` syscall、`kernel/src/syscalls/exec_wasm.rs`、`tools/unix` crate、`agent/src/tools.rs`、`wasm.toml` manifest 全部刪除。`list_dir`/`make_dir`/`delete_path` 三個 action 直接在 `agent_loop.rs` 用 `std::fs`,跟 `read_file`/`write_file` 同款寫法。這條路留著記錄是因為「wasmtime component model host 一個真正的 wasip2 tool 跑得通」這個技術驗證本身是有效資訊——之後如果真的要跑不信任的第三方程式碼,這套做法已經驗證過可行,不用重新摸索;只是現在沒有這個需求,先不留著閒置的複雜度。

### 4.8 Credential Vault(secret 永不進沙箱)
不新增 syscall,做在 `http_get` 邊界:

1. Kernel 端 `secrets.toml`(**存於 agent-home 外**),gateway 管理
2. 每個 secret 綁定 domain:`github_token → api.github.com`
3. Agent 用佔位符:`Authorization: Bearer {{secret:github_token}}`
4. Kernel 驗證 secret ↔ domain 綁定 → 通過才注入真值發出;不符則拒絕 + notify

**性質**:agent 從頭到尾沒看過明文,無物可洩——比 egress leak scanning 更強的保證。Injection 最多騙 agent「把 token 用在 evil.com」,domain 綁定直接擋下。Response 寫回 log 前掃一次 secret 值以防 API 回聲(唯一需要掃描的點)。

### 4.9 `ssh_exec`(唯一一個接近真 shell 的 syscall)
使用者明確要開:讓 agent 能 SSH 到一台 docker linux 做 git 開發等事情。這條路先前討論「要不要給 agent 真 shell」時明確否掉過(見 4.6 威脅模型),這次是使用者知情後主動要求開,範圍設計上盡量收斂:

- **連線目標寫死**:`config.toml` `[ssh] host/port/user`,agent 不能自己選——防 injection 騙去接到別的主機
- **金鑰不進沙箱**:`secrets.toml`(agent-home 外)`ssh_key_path`(+ 可選 `ssh_key_passphrase`),guest 只看得到 config.toml 裡的連線參數,看不到金鑰路徑對應的檔案內容(WASI preopen 就是 agent-home,金鑰檔案在外面)
- **不開互動式 shell/pty**:單次指令進、單次 `{stdout, stderr, exit_code, timed_out}` 出,同 `db_exec`/`http_get` 的一次性呼叫形狀
- **硬 wall-clock deadline**(`timeout_secs`,`Instant` 算,每次 read 都檢查,不是只靠 libssh2 的 idle timeout):討論時特別想到「agent 跑 `docker logs -f` 會怎樣」——這種指令不會自己結束。**現況**:鎖現在是 per-session(見 §4.5),一個卡住的 `ssh_exec` 只凍結它自己那個 session,不再凍結全域每個 surface;但這個 deadline 還是需要——沒有它,那個被卡住的 session(或沒有 session 的背景喚醒)自己永遠跑不完,一樣是壞的。所以 deadline 是每次 read 都重新檢查「有沒有超過總時限」,不是「多久沒新資料」——一直有資料進來的 `-f` 一樣會被砍
- **輸出上限**(`max_output_bytes`):防洗版塞爆 context/log
- **全量 audit log**(`logs/ssh.jsonl`:指令、exit_code、位元組數、timed_out、來源),連 `not_configured` 這種還沒連線就失敗的也記,同 `http_get` 的 egress log 精神(被拒的也要留痕)
- 沒設 `[ssh].host` 或沒有 `ssh_key_path` secret → `not_configured`,同 Discord adapter 的「沒設 token 就整個不連」模式

**containment 的性質**:防的是「目標被騙走」「卡死自己那個 session」「洩漏憑證」「無稽核」,不是「指令本身能幹嘛」——一旦連上那台機器,那個 SSH user 能做什麼,這個指令就能做什麼,跟 `db_exec`(SQL authorizer 鎖操作種類)或已經刪除的 `exec_wasm`(wasm 沙盒鎖能碰的目錄)不是同一個防線層級,純粹是「範圍越小、稽核越全,炸掉的半徑越小」。使用者知情選擇,目標建議是可拋棄的 dev container,不是真正有資料的機器。

依賴:`ssh2`(libssh2 bindings,vendor 編譯,不需要系統裝 libssh2)。

---

## 5. 關鍵取捨(已定案)

| 決策 | 選擇 | 理由 |
|---|---|---|
| SQLite 位置 | host 原生 + `db_exec` syscall | WASI guest 端無共享記憶體/完整檔案鎖,WAL 在 guest 端不可用;host 端 FTS5/sqlite-vec 全原生。代價:必須做 authorizer 加固。**現況更新**:host 端這顆 `index.db` 後來自己開了 WAL mode(`kernel/src/state.rs`)——理由不是效能,是讓外部工具(DB Browser for SQLite 之類)開著檔案時不會跟 agent run 搶 rollback journal 的 exclusive lock,搶 lock 這件事真的發生過 |
| 程式碼執行 | 延後 | 先看 agent 純靠檔案 + llm_call 能活成什麼樣;之後要加,走「python.wasm 作為插件」或 rootless container 再議 |
| wasip1 + JSON | 是 | 避開 Component Model 前期成本;曾為 `exec_wasm` 工具沙盒另外加過 wasip2 component 支援,後來連 `exec_wasm` 本身都回退掉了(見 4.7)——目前全專案只有主 agent 這一個 wasmtime Store。**這次會話重新被問過一次**「wasip2 不是越新越好嗎」,結論沒變:自訂 `env.syscall` 單一 generic import + JSON envelope 這套協定就是為了避開 component model 那筆前期成本才選的,沒有具體要用的 p2-only 功能之前,換的投報率仍然偏低;wasip3(原生 async)更不成熟,現在架構每個 run 一個 Store、同步跑到底,用不上 |
| fresh instantiate | 是 | crash-safe、零殘留。**曾經改成、後來退回**:一度把每次喚醒改成真的獨立 child process(不只是同 process 內 fresh `Store`),換取 `POST /api/abort` 能對這次 run 直接 `SIGKILL`、不管卡在哪個 syscall。後來評估:這個保證換來的部署代價(多一個要跟著部署的 binary、兩個 binary 版本可能兜不上,即使後來改成 `current_exe()` re-exec 自己也沒有完全消掉這個複雜度)不值得,退回原本這個簡單版本——`POST /api/abort` 現在只是 cooperative flag,卡在 `http_get`/`ssh_exec` 的 run 攔不住,run 到完為止。跟這個決定連動的另一件事:`run_lock` 從全域一把鎖改成 per-session(見 §4.5),讓「同時只能跑一個」這個舊限制本身也一起鬆開了 |
| 前端 | ~~無 build 系統單頁~~ → 獨立 Vite+Vue 專案(`webui/`),真前後端分離 | 原本圖雜務最小化;使用者後來明確要求前後端分離、元件粒度更細,翻案換取這個 |

---

## 6. TODO List

### Phase 0 — 骨架(~1 天)
- [x] repo:`kernel/`、`agent/`、`agent-home/`
- [x] wasmtime 載入 hello-world .wasm 並執行
- [x] guest Rust → wasm32-wasip1 build 通
- [x] preopen 打通:guest 在 `/` 讀寫 → 落在 `agent-home/`
- [x] 隔離測試:`..`、絕對路徑、symlink 逃逸全失敗(寫成測試)

### Phase 1 — Syscalls(~3 天)
- [x] 六個 syscall 的 JSON schema(request/response/error)— 統一 envelope,見 `kernel/src/abi.rs`
- [x] `llm_call`(reqwest 打 API,key 從 host env)+ token 記帳 → `logs/usage.jsonl`
- [x] **每日預算硬上限**:config 設額度,llm_call/embed 超額直接拒絕 + notify
- [x] **llm_call rate limiter**(token bucket):block ≤3s → 仍無配額回 rate_limited;打頂 notify
- [x] **Transcript log**:每次 llm_call 完整 prompt/response 落 `logs/transcripts/`
- [x] **llm_call 重試 + circuit breaker**:單次呼叫內重試 5 次、指數 backoff(500ms/1s/2s/4s,最多多等 ~7.5s)——**所有錯誤都重試**,連線失敗跟 HTTP 4xx/5xx 都算(使用者明確要求,知道有 non-idempotent resend 風險但接受)。再上一層是 circuit breaker(`logs/llm_circuit_breaker.json`,獨立於 `AgentState` 之外,因為每次 run 都是全新 instantiate,這個要跨 run 存活):連續 3 次「整次呼叫的 5 次內部重試全部用完」才跳,跳開後 60 秒內直接拒絕(`circuit_open`),不再浪費時間重試一個大概率真的掛掉的 API;成功一次就清空計數。真跑驗證過:指向不存在的位址,3 次呼叫各花 ~8.5-9.5s(重試生效),第 4 次 ~1s 內直接被 circuit_open 擋下,換回真 API 後第一次呼叫成功並自動清空 circuit 狀態。
- [x] `embed`
- [x] `db_exec`:原生 SQLite 開 `memory/index.db` + **authorizer 加固**(禁 ATTACH/load_extension/危險 PRAGMA)+ **查詢逾時(config,預設 10s)**
- [x] `notify` / `sleep_until`
- [x] 資源限制:epoch 逾時、memory cap、env 清空、stdio → log
- [x] 安全測試:db_exec 嘗試 ATTACH `/etc/passwd` → 被拒

### Phase 2 — Agent Loop(~3 天)
- [x] 讀 `config.toml` + 記憶 → 組 system prompt
- [x] 行動 JSON 解析與多輪執行迴圈
- [x] 記憶寫入:結束前更新 `notes/`
- [x] **Auto-commit**:每次執行結束 kernel 對 agent-home 做 git commit(大腦時光機;搞壞記憶可 rollback)
- [x] 端到端:給任務,觀察跨多次喚醒的行為

### Phase 3 — RAG 記憶(~3-4 天)
- [x] chunks schema + FTS5 虛擬表(guest 端 `ensure_schema`,contentless FTS5,見 `agent/src/memory.rs`)
- [x] chunking:markdown-aware(標題邊界 + overlap)
- [x] hybrid 檢索:BM25 + 向量(BLOB/TEXT cosine,筆數少暴力法)→ RRF → top-k
- [x] agent loop 接上讀路徑
- [x] `daily_maintenance` cron:蒸餾 → hash 增量重嵌 → LLM 整理 → report(cron 排程本身是 Phase4 scheduler 的事,trigger 處理已就緒)
- [x] embed_model 變更 → 全庫重嵌
- [x] 模擬多天連跑,驗證記憶累積與檢索命中

### Phase 4 — Gateway + 前端(~3-4 天)
- [x] axum:message / status / logs(SSE)(outbox 概念用 `/api/message` 同步回傳 result 取代,還沒有非同步 outbox 佇列)
- [ ] grant 流程:`request_external` 掛起 → 審核 API → 複製進 `inbox/granted/` → 喚醒 → audit log(**使用者明確決定先不做**——`http_get` 自己的 grant queue(tofu 新 domain 審核)已經覆蓋大部分「網路動作要人審」的需求,`request_external` 這條專用管道暫緩)
- [ ] grant TTL + 單次性 + config 白名單免審(同上,延後)
- [x] 前端單頁:聊天、log 面板、記憶瀏覽器(無 grant 卡片,功能還沒做)
- [x] 控制面板部分:wake now(`/api/wake`)。**pause(soft/hard)/resume 使用者明確決定不做**——4.5 有 scheduler loop 也有 `run_lock` 排隊了,但沒有「暫停」開關本身;`/api/abort` 已經涵蓋「中斷正在跑的這一次」的實際需求,soft/hard pause 這種更複雜的排程層開關暫不追
- [x](簡化掉)Kernel config 不需要 `ArcSwap`:`Config::load` 本來就每次喚醒重新讀檔,沒有跨喚醒快取,寫入 `/api/config` 下次喚醒自動生效,不需要熱重載機制
- [ ] hard stop 測試:依賴 pause/resume,不做了故此測試也不用補
- [x] token 認證(單一 bearer token,`/` 首頁例外;SSE 因瀏覽器 EventSource 限制額外接受 `?token=` query)
- [x] 前端嵌進 binary(`include_str!`,單一 html 檔,無 build 系統)
- [x] **新增(非原清單)**:`POST /api/config` 寫入前先驗證 TOML 能 parse 進 `Config` struct;`POST /api/secrets` 只寫不讀——回應只給 vault 內的 key 名稱清單,沒有任何 endpoint 能讀出 secret 值

### Phase 5 — 網路與工具自動安裝(~2-3 天)
- [x] `http_fetch`:reqwest 代發;DNS 解析後檢查 IP 落點(`resolve()` 釘住,防 rebinding TOCTOU)
- [x] Denylist:localhost / RFC1918 / link-local / metadata endpoint
- [x] Egress log(**完整 URL** + bytes)→ gateway 可查(`GET /api/egress`)
- [x] GET 模式模組:`open`(預設)/ `tofu` / `allowlist`,config 切換
- [x] `url_max_len` + `daily_request_cap` 限制
- [x] **http rate limiter**:全域 + per-domain token bucket(語義同 llm_call)
- [x]→removed POST/上傳審核佇列——做過(`grants.rs` `http_write` + `GET/POST /api/grants`),後來拿掉了(見 4.6 現況更新,`ssh_exec` 出現後這道審核不再是真防線);`grants.rs`/`GET/POST /api/grants` 本體還在,tofu 新 domain 審核繼續共用
- [x]→[~] `exec_wasm`:新 Store、只 preopen workspace、無網路、fuel/memory cap——建好測過,後來整個回退了(見 4.7 現況更新),換成 `list_dir`/`make_dir`/`delete_path` guest-side action
- [ ] 安裝驗證:wasm module 合法性 + size cap;寫入 lockfile.json(**使用者明確決定不做**——原本設想走 wapm 生態自動裝工具,現在 wapm 已不是活躍選項,agent 自動安裝工具這條路線本身不追了;exec_wasm 本體單獨可用不受影響)
- [x] agent-home 磁碟配額 + 超額 notify:原本評估要「更深的 wasmtime-wasi hook 才能攔寫入」,後來發現不用——`write_file` 本來就是 guest-side `std::fs`(唯一會讓磁碟變大的 action),直接在 `agent_loop.rs` 裡量 agent-home 總大小(遞迴走 `/`),寫入前檔會讓總量超過 `config.toml` `[disk] quota_bytes`(預設 2GB,`kernel/src/config.rs` `DiskConfig`)就拒絕 + `notify`,不寫。真跑驗證過:quota 設 1000 bytes,write_file 被拒且 notify 正確記錄訊息;quota 還原預設後同一個 write_file 正常成功。
- [x] gateway 前端加 egress log 面板(`webui/src/components/EgressPanel.vue`,`GET /api/egress`)。已安裝工具列表(lockfile)面板延後——lockfile 本體(上一行)還沒做,沒東西可列
- [ ] Credential vault:secrets.toml(agent-home 外)+ gateway 管理頁(現有 vault 只支援 llm/embed 用的 `{secrets.x}`,domain 綁定機制延後)
- [ ] 佔位符注入:`{{secret:name}}` 展開 + domain 綁定驗證(延後)
- [x] Response 落 log 前掃 secret 值(防 API 回聲)——`http_get` 的 `redact_secrets` 已做,對象是現有 vault 的值
- [ ] 測試:secret 用在未綁定 domain → 拒絕(domain 綁定機制延後,無法測)
- [x]→removed 測試:`exec_wasm` 跑通(用純 WASI 測試二進位而非真裝 jq.wasm)、工具讀不到 `memory/`——`exec_wasm` 回退後這兩個測試也一起刪了;`http_get` 打 `192.168.x.x`/`127.0.0.1`/`169.254.169.254` 全被拒(這條還在)

### Phase 6 — 品質債(2026-07-09 assessment,今天一天連續修出至少 6 個實際 bug 後補記)
- [ ] `agent/` crate 補單元測試——目前 `agent/src/*.rs` **零測試**,今天改動最密集的
      `agent_loop.rs`(`recent_log_entries`/`write_memory_note`/llm_call 失敗重試邏輯)全靠手動跑
      live server 驗證。`kernel/` 有 23 個 test 撐著,`agent/` 這邊裸奔——優先度最高,今天的 bug
      密度就是訊號
- [ ] `cli/`(`ebinactl`)補測試——目前零測試
- [x] `PROJECT.md` 同步實際程式碼(2026-07-11 補做,同天內 subprocess+SIGKILL abort 那段又
      改回 cooperative-only + per-session lock,一併同步):`memory_get`/`memory_set` 已從全文
      拿掉、`http_fetch`→`http_get` 改名已同步、補了 `ebinactl` 現況(不再是「尚未做的 CLI
      wrapper」)、`daily_maintenance` 6 小時循環、streaming、write_file 依 trigger type 限定範圍、
      read_file 分頁/`grep_file`、mid-run compact、http_get cache、WAL mode、`db_exec` PRAGMA
      allowlist 那個 bug、`run_lock` 全域鎖改 per-session,§2/§3/§4.2-4.6/§5 都補了
- [ ] LLM provider 單點依賴——目前只有一個 provider,該 provider 掛掉會連鎖影響
      daily_maintenance/cron 等背景喚醒(Moonshot 斷線是真實發生過的案例);評估要不要 fallback
      provider,或至少現況接受、記錄下來當已知限制
- [x] 單一 run 內沒有主動式 context 監控/壓縮(2026-07-11 補做):`runtime.in_run_compact_tokens`
      ——每輪 llm_call 回來的 `input_tokens` 超過門檻(預設 150,000)就在 run 內自己觸發一次 compact,
      system prompt + 最初任務訊息保留,中間摘要掉,跟 `daily_maintenance`/`auto_compact_tokens`
      是三件不同的事,見 §4.2。真跑驗證過:藏一句密語在最初訊息,兩次 compact 後依然完整複誦
- [x] session lock race 全面稽核(2026-07-11,`run_lock` 全域鎖改 per-session/per-task_id 後連續
      多輪稽核揪出來的)——每項都真跑併發實測過(見各 commit message 的數字),不是純 code review:
  - [x] `budget.rs` token 計數 lost update(FileLock)
  - [x] `memory.rs reindex_file` 重複 chunk row(embed 留在 transaction 外,`BEGIN IMMEDIATE` 前
        重查 hash,optimistic concurrency)
  - [x] `autocommit.rs` commit 衝突(FileLock 鎖整個函式)
  - [x] `http_get` cache 寫入半毀檔(temp file + rename)
  - [x] guest 端 disk quota TOCTOU(`DiskQuotaLock`,guest-side `create_new` busy-retry)
  - [x] `logs.rs append_jsonl` 交錯寫入(`writeln!` 改成組 String 後一次 `write_all`)
  - [x] `ratelimit.rs` 改真正全域共用(每個 run 各自一份 bucket → `OnceLock<GlobalRateLimiters>`)
  - [x] `grants.rs` request/approve/deny lost update(FileLock)
  - [x] `scheduler_tasks.rs` add/update/mark_run id 衝突與互蓋(整目錄鎖 + per-task 鎖)
  - [x] 同一 `scheduled_task` 被 cron tick + 手動 `/api/wake` 同時觸發(`task_run_locks`,
        per-task_id lock,仿 `session_locks` 同款設計)
  - [x] `session_locks`/`task_run_locks` 加 idle sweep(防長跑 process 記憶體慢慢漲)
  - [x] `/api/abort` 從全域 flag 改 per-session(`?session=`,跟 `thinking_path` 同一套 key 慣例,
        webui `ChatPanel.vue` 不用改)
  - [x] **最嚴重那個**:`chat_send`(背景 trigger 主動推訊息)vs `handle_chat_message` 蓋寫
        `session.json`——`session_locks` 只鎖 trigger 自己的 session,鎖不到 `chat_send` 打的目標
        session,原本會整包蓋掉對方剛寫的一輪對話。修法:`handle_chat_message` 結尾改成重讀最新
        狀態再 append 這輪的 delta,兩邊都上同一把 `session.json.lock`
  - [x] `llm_call.rs` circuit breaker(全域檔案,無 session key)lost update(FileLock)
  - [x] `secrets.rs` `post_secret` lost update(FileLock,跟 grants.rs 同款病)
  - [x] `persist_last_run` 破損 JSON(temp file + rename)
  - [x] `skills.rs` `record_use`(guest)vs `post_skill`(host)雙邊 lost update——唯一一個橫跨
        guest/host 邊界的修法:guest 端 `SkillLock`(`create_new` busy-retry)+ host 端 `FileLock`,
        鎖同一個實體路徑(`<name>.md.lock`),兩邊互通
  - [x] `index.db` 補 `busy_timeout`(原本 0)——不是資料損毀問題,WAL 模式本身已保證正確,但併發
        寫入者現在會立刻 `SQLITE_BUSY` 失敗,是 pivot 後才會出現的新失敗模式,設 5s
  - [ ] backlog(低風險,記錄但沒修):`save_attachment`/`put_task_file` 檔名或 quota 的 TOCTOU
        (last-write-wins,非損毀,人為操作機率低);`compact_session_key`/`reset_session_key` vs
        `chat_send` 的模糊地帶(語意上本來就是清掉舊歷史,重疊到算可接受);`discord.rs`
        `pairing_seed()` 全新 agent-home 第一次呼叫的 TOCTOU(配對碼本來就不是安全邊界)
  - [x] 順便:`WasmRuntime`(Engine/Linker/Module 只建一次,只有 Store 需要每次 fresh)——跟這輪
        稽核同期但不是 race fix,是效能選項,`[runtime] cache_wasm_module`(預設 `false`),見 §4.2

### 未來糖果罐(延後)
- [ ] **Agent 互通(A2A,actor model)**:設計已定——新 syscall `send_agent(target, msg)`,kernel **複製**訊息至對方 `inbox/from-<sender>/` 並喚醒;不共享任何目錄,Store 間零接觸;通訊拓撲在 kernel config 逐條宣告(capability),未宣告組合拒絕;訊息全經 kernel = 全量 A2A log,gateway 可視化對話圖。支援監督者模式、互相 review 等玩法;新 agent = 新資料夾 + 一行拓撲
- [ ] 多 agent 基礎(每 agent 一個 Store + 資料夾,scheduler 泛化)
- [ ] wasip2 / Component Model 遷移
- [x] **Discord adapter**(接在 gateway 上,不動 kernel——`kernel/src/discord.rs`,serenity crate 跑 Gateway websocket):只回 DM 或 @mention(避免頻道裡隨便講話都觸發);每個 DM/頻道各自一份 session(`discord-dm-<user>`/`discord-channel-<channel>`,存在 `logs/chat_sessions/<key>/`),不跟 webui 的 `webui` session 混;RAG/SOUL/skills/scheduled tasks 全域共用,只有原始對話串分開。沒設 `discord_bot_token` secret 就整個不連,gateway 照常運作。連帶把 session 儲存從單一寫死路徑改成 keyed(`session_dir(agent_home, key)`),為多 channel 鋪路
  - [x] session compact/reset 泛化成 keyed(`gateway.rs` `compact_session_key`/`reset_session_key`,原本寫死 `"webui"`),webui 兩顆按鈕跟 Discord `!compact`/`!reset` 指令共用同一套;Discord 沒有前端按鈕可按,另外加一個 auto-compact:單一 session 的 `context_tokens` 超過 `config.toml` `[chat] auto_compact_tokens`(預設 50000)門檻,下次那個 session 一有新訊息就在背景自動 compact,不擋當次回覆。`session_watch_loop` 原本 `turns.len() <= last` 沒處理 session 被 compact/reset 縮短的情況,下次成長超過舊 `last` 會 slice 越界 panic——已修成偵測到變短就重新 baseline
- [ ] Telegram adapter(接在 gateway 上,不動 kernel)——跟上面 Discord 同一套改法,概念已驗證過
- [ ] python.wasm 作為標準工具(module precompile cache)——層次一自主開發:agent 寫 Python、exec_wasm 跑、迭代

### 里程碑
- **M1**(P1 完):guest 經 syscall 完成一次 LLM 對話 + DB 寫入
- **M2**(P2 完):agent 自主多輪任務 + 管理自己的筆記
- **M3**(P3 完):每日維護跑起來,記憶檢索命中
- **M4**(P4 完):瀏覽器裡跟 agent 對話、看它的腦、核准它的請求
- **M5**(P5 完):agent 自由上網查資料、自己下載工具並用 exec_wasm 跑起來

---

## 7. 安全檢查清單
- [x] preopen 逃逸測試通過(traversal / symlink)
- [x] 沙箱內 env 為空
- [ ] API key 不出現在任何 agent 可讀路徑或 log
- [x] db_exec:ATTACH / load_extension / 危險 PRAGMA 全被拒
- [x] 逾時與記憶體上限實測觸發
- [ ] grant:只複製、無額外 preopen;audit log 完整(請求理由/核准人/時間)(request_external 本體延後,grant 佇列機制已用於 http_get)
- [x] gateway token 認證生效
- [ ] agent-home 整個刪除 → host 不 crash
- [x] http_fetch:私網段全被拒(localhost/RFC1918/link-local/metadata);egress log 無遺漏(DNS rebinding 情境靠 resolve()-then-pin 結構性防護,未特別造一個會 rebind 的測試網域)
- [x] POST 未經核准不會發出:實測 queue → approve → 同一請求重放才真的發出
- removed exec_wasm 的 Store:看不到 memory/(只有 workspace)實測——`exec_wasm` 本身已回退,這項保證不再適用(見 4.7)
- [x] 磁碟配額實測觸發:quota 設 1000 bytes,write_file 正確拒絕 + notify;還原預設後正常寫入
- [ ] vault:agent 全世界內 grep 不到任何 secret 明文;domain 綁定不符即拒(domain 綁定延後)
- [ ] 預算硬上限實測:超額後 llm_call 被拒且 notify
- [x] db_exec 逾時實測:慢查詢被砍,kernel 不卡死
- [x] url_max_len / daily_request_cap 實測觸發
- [ ] rate limiter 實測:模擬失控迴圈 → 3s 內節流放行或收到 rate_limited,且 notify 觸發
- [x] auto-commit 可 rollback:手動污染 notes/ 後 git checkout 還原