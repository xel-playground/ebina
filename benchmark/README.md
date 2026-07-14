# ebina 記憶瀑布 benchmark

adapted from LOCOMO(Evaluating Very Long-Term Conversational Memory of LLM
Agents)的方法論——種事實進對話、測retrieval——但改成適配 ebina 的三層瀑布
式蒸餾架構,加了 LOCOMO 沒有的「分層準確度」測試(這是 001-memory-v2.md 設計
理念真正的賭注:重要性判斷準不準)。

## 為什麼可行:無狀態架構

`AgentState` 每次 run 都是全新的(見 kernel/src/state.rs),沒有記憶體殘留狀
態。唯一重要的狀態是硬碟上 agent-home/ 底下的檔案。這代表整個系統是
`(硬碟狀態, trigger) -> (新硬碟狀態, outcome)` 的純函數——測試只要能控制硬
碟狀態跟 trigger,結果就可重現、可驗證。

## 怎麼模擬多天而不用真的等

三層蒸餾(1h/6h/24h)在正式環境靠 `scheduler_loop` 的 wall-clock gating 才會
自動觸發。benchmark 直接繞過這層,用 `POST /api/wake` 帶對應的
`{"type": "daily_maintenance"/"maintenance_summary"/"core_distillation",
"since_ts": N}` 直接觸發——這是今天在正式環境手動測 core_distillation 驗證
過可行的方法。`since_ts` 只是「只審查這個時間點之後的內容」的邏輯 checkpoint,
不是真的要等經過那麼多秒——所以「模擬過了三天」實際上是「seed 三次對話、中
間各跑一次 daily_maintenance,checkpoint 照實際完成時間推進」,壓縮在幾分鐘
內跑完,但每一層都是真的 LLM 在判斷,不是假結果。

## 資料夾

```
benchmark/
├── setup_testbed.sh      建一個乾淨、隔離的 agent-home(絕不碰 tmp/main)
├── lib.py                共用 helper(啟動/停止 testbed、seed、觸發、讀檔、評分)
├── run_benchmark.py       主執行腳本
├── scenarios/*.json       題目定義
└── results/*.json         每次執行的完整結果(timestamp 命名)
```

testbed 本身建在 `tmp/benchmark-testbed/`(被 `.gitignore` 的 `/tmp/` 規則
排除),用中立的測試用 SOUL.md(不沿用正式環境的蝦奈人設)+ 複製正式環境的
config.toml/secrets.toml(同一個 LLM/embed provider,測的才有意義)但從空白
notes/core.md/skills 開始。

## 執行

```bash
python3 benchmark/run_benchmark.py                        # 全部題目
python3 benchmark/run_benchmark.py single_hop              # 單一題目
python3 benchmark/run_benchmark.py tier_accuracy adversarial
```

每個 scenario 執行前都會重置 testbed(乾淨石板,不同 scenario 不互相污染)。

**這 4 個是核心正確性驗證套件,目的是「記憶系統有沒有壞」,不是拿去跟 Mem0/Zep
比較**——真要跟業界對標,需要真的 LOCOMO 官方資料集 + LLM-as-judge 評分 + 大樣本,
成本高很多,對單人自用的 agent 沒有實際必要,已經評估過決定不做(見下方
`locomo_adapter.py` 一節)。這 4 個 scenario 平常改完 prompt/蒸餾邏輯後重跑一次
當回歸測試就好。

## 題型

| 檔案 | 對應 LOCOMO | 測的是什麼 |
|---|---|---|
| `single_hop.json` | 單跳 | 最基本的 retrieval 有沒有動 |
| `tier_accuracy.json` | (ebina 專屬,LOCOMO 沒有) | 重要性判斷準不準——瑣碎的別進 core、待辦別進 notes、真偏好要爬進 core |
| `temporal_override.json` | 時序/覆蓋 | 後面的事實有沒有正確蓋掉前面過時的 |
| `adversarial.json` | 對抗 | 沒提過的事,會不會瞎編(測 precision 不是 recall) |

## 加新題目

照現有 json 的格式:`days`(每天要 seed 的訊息陣列)+ `checks`(驗證規則)。
`checks` 支援的 `type`:`core_includes`/`core_excludes`/`notes_includes`/
`notes_excludes`/`workspace_includes`(直接讀檔比對關鍵字,不分大小寫)、
`ask`(真的問一題,比對答案內容)、`ask_uncertain`(問一題沒提過的事,驗證有
沒有正確表達不知道,而不是瞎編)。

## 已知限制

- 只測了 1-2 天份的場景,還沒有測「同一件事重複出現 3 次才升級」這種需要更
  多 cycle 的場景(待辦升級的 6h escalation、skill 品質閘門)
- 每次跑都是真的打 LLM/embed API,花真的 token/錢,不是免費的 mock 測試
- `ask_uncertain` 的判準是關鍵字比對(「不知道」「沒有印象」...),不是語意
  判斷,模型換句話說可能漏抓——之後可以考慮換成 LLM-as-judge

## `locomo_adapter.py`(擱置,不是核心套件的一部分)

`locomo_data/` 底下放真實 LOCOMO 資料集(需自行下載
`snap-research/locomo` 的 `data/locomo10.json` 到這裡)+ 一支轉換腳本
(`locomo_adapter.py`),能把官方資料轉成這套 benchmark 吃的格式。**評估過後決
定不追加使用**——這條路要做到位需要:(1) 把雙人對話硬轉成「轉述給 ebina」的
框架,跟 ebina 實際「單一主人講自己的事」場景不同;(2) 需要真的模擬 session 之
間橫跨數週的時間流逝(patch `log.md` 的 ts),不是壓縮在幾分鐘內跑完;(3) 需要
LLM-as-judge 評分取代關鍵字比對。三個都要做才有意義,但買到的東西主要是「能
跟 Mem0/Zep 論文數字比較」——對單人自用工具沒有實際必要,成本(真金白銀的 API
費用)不划算。腳本留著,想清楚要不要做學術對標時再撿回來用,不是現在的優先項。
