# アーキテクチャ

Nelfie は Rust 製の Discord bot です。Discord Gateway と OpenAI Responses API / Decisions API を中心に、コマンド、会話、ツール実行、VOICEVOX 読み上げをひとつのプロセスで動かします。

## 起動と終了

`src/main.rs` がプロセスのライフサイクルを管理します。

1. `.env` を読み込み、ログと rustls の暗号プロバイダーを初期化します。
2. `NelfieContext` が設定と共有サービスを構築します。各サービスの詳細は次の節を参照してください。
3. TCP 監視サーバーを bind して別タスクで起動します。bind に失敗すると起動を中止します。
4. Discord bot を開始する前に VOICEVOX core を初期化します。
5. `discord::bot::start` が Poise、Songbird、イベントハンドラーを設定し、slash command を登録して Serenity Gateway を別タスクで開始します。Gateway の接続状態は `Ready`、再接続、切断イベントから監視状態へ反映します。
6. Ctrl-C を受けると、進行中の応答を中止し、cron と音声状態を停止・クリアしてから監視サーバーを終了します。

`NelfieContext` は `Arc` を通して共有されます。クローンは各サービスの所有データを複製せず、同じ共有状態を参照します。

## モジュール

| パス | 責務 |
| --- | --- |
| `src/app/` | 環境設定、共有コンテキスト、cron scheduler、監視状態と TCP 監視サーバー |
| `src/discord/` | Serenity / Poise の起動、イベント振り分け、コマンド、応答・添付ファイル配送 |
| `src/llm/` | Responses API / Decisions API クライアント、会話履歴、ユーザー設定、モデル、ツール |
| `src/voice/` | VOICEVOX core、読み上げテキスト、辞書、チャンネル別キュー、音声再生 |
| `src/logging.rs` | ログ初期化と出力整形 |

Discord の slash command は `src/discord/commands/` に実装し、`command_registry::all` から登録します。LLM tool は `LMTool` を実装し、`src/llm/tools/mod.rs` の `registry` に登録します。Gateway event は `src/discord/events.rs` からメッセージ、interaction、voice event などの処理へ振り分けます。

`discord-tool` は検索・管理などの汎用Discord操作を担い、`discord-reply` は会話メッセージの送信を担います。後者は1回のtool callで1件だけ現在のチャンネルへ投稿し、返信先の省略時は応答の起点になったメッセージへ返信します。連続投稿では前回のtool結果に含まれる `message_id` を次回の `reply_to` に指定できます。メンションはユーザーを指定した場合だけ許可されます。

## 状態の所有者と永続化

| 所有者 | 主な状態 | 保存先 |
| --- | --- | --- |
| `Config` | API token、system prompt、rate limit、管理者 ID、VOICEVOX と監視の設定 | 非秘密設定は `config.yaml`、API token は環境変数または `.env` |
| `ChatContexts` | チャンネルごとの会話履歴、prompt、機能フラグ、読み上げ辞書 | `data/runtime/chat_contexts.json` には設定のみ保存。会話履歴はプロセス内だけ |
| `UserContexts` | ユーザー別モデル、unlimited 設定、話者・音声設定、個人辞書。guild bot 用設定も管理 | `data/runtime/user_contexts.json` |
| `CronScheduler` | 登録済み cron job と次回実行時刻 | `data/runtime/cron_jobs.json` |
| `VoiceSystem` | core runtime、Songbird 接続、guild 接続設定、チャンネル別キュー | 接続とキューはプロセス内 |
| `MonitoringState` | 起動時間、Discord Gateway と VOICEVOX の準備状態 | プロセス内 |

JSON ファイルのパスは実行時の working directory からの相対パスです。各 store の書き込みは同一 store ごとに直列化し、同じディレクトリの一時ファイルを完成させてから置き換えます。`ChatContexts` は会話履歴を JSON に書き出しません。`UserContexts` は無制限を示す `rate_line=0` と `cron_rate_line=0` を保存しますが、正の値は保存しないため、再起動時に `1` へ戻ります。

管理者専用の `/rate config`、`/set_system_prompt`、`/enable mode:auto-reply` は `config.yaml` の `admin_users` に指定した Discord user ID で制御します。未設定または空なら許可される管理者はいません。ChatContexts は選択した応答モードをチャンネル設定として保存し、旧データは `mention` モードとして読み込みます。

全体読み上げ辞書は `include_str!` で `global-dict.json` をビルド時に取り込みます。VOICEVOX の辞書・モデル・ONNX Runtime の配置先は `config.yaml` の `voicevox_open_jtalk_dict_dir`、`voicevox_vvm_dir`、`voicevox_onnxruntime_filename` で指定します。

## メッセージから応答まで

全体の分岐と処理順は [AIチャットの処理フロー](chat-flow.md) を参照してください。以下では各構成要素の動作を記します。

`discord::message_events::handle_message` は、チャンネルの自動読み上げが有効なら先に読み上げを要求します。その後、発言者・本文・添付ファイル情報を `LMContext` に追加します。画像の URL と元メッセージ・添付 ID も保持し、Responses API への送信直前に必要なら Discord から URL を更新します。

bot への mention があり、対象チャンネルの ChatBot が有効な場合に `responses::schedule_message_response` が応答タスクを作成します。`active_responses` はチャンネルごとに最新のタスクを保持し、後から来た応答要求は前のタスクを中止します。`run_response_task` がレート制限、system prompt、OpenAI 呼び出し、Discord への結果配送を扱います。

`LMClient::generate_response` は streaming Responses API を使い、関数 tool の実行結果を次の API round に戻します。tool 定義は `LMTool` と registry から取得し、Web Search はクライアント側で追加します。画像生成はモデル設定後の API round で有効にします。生成した応答の履歴はチャンネル文脈へ統合します。ユーザー起点の応答要求がレート制限を通過した後、API生成前に行う履歴の選択と非同期圧縮は次節のとおりです。

## 会話履歴のコンテキスト圧縮

### 起動条件と通常応答の入力範囲

レート制限を通過したユーザー起点の要求（メンションまたはモーダル）で、OpenAI 応答を始める前にチャンネル履歴を確認します。見積りが `config.yaml` の `context_compaction_token_limit` 以上なら、古いturnの圧縮をバックグラウンドで開始し、通常応答と並行して進めます。受信しただけのメッセージ、レート制限で拒否された要求、cron応答は圧縮を開始しません。既に始まった圧縮は、利用者が発言を止めた後も完了またはtimeoutまで続くことがあります。

要求に使う履歴は全件を送りません。`LMContext` の各 item を JSON 化し、`tiktoken-rs` の `o200k_base` で数えた推定値が、しきい値の75%以下になるよう古いuser turnから外します。System promptとdeveloper roleの要約は保持し、少なくとも最新turnは残します。この切り出しは要求用のコピーだけに適用し、チャンネル履歴自体は削除しません。画像1件につき1024 token分を追加で見積もります。

`o200k_base` は明示指定です。現在の `tiktoken-rs` はGPT-6用tokenizerをモデル名から解決できないため、この数値はAPIの `usage.input_tokens` と一致する保証値ではありません。画像やtool定義などの差もあるため、推定に余裕を持たせています。最新turnと保持対象の指示だけで上限を超える場合は、それらを捨てずに送るため、この上限は絶対値ではありません。

`config.yaml` に値がない場合、`context_compaction_token_limit` は `64000`、`context_summary_max_output_tokens` は `6144` です。したがって通常応答の履歴目標は既定で48,000推定tokenです。Responses API が返す `usage.input_tokens` は応答記録用であり、圧縮開始・継続の判定には使いません。

### 要約する範囲

圧縮は古い履歴から連続した turn 単位の batch で行います。turn は `user` メッセージから次の `user` メッセージ直前までで、assistant と tool の応答を含みます。最新の2 turnは残します。1 batch の要約入力は、画像をテキスト marker に置き換えた後の `InputParam` JSON 表現で最大32 KiBです。上限内に収まる古い turn を順に追加します。最初の対象 turn が上限より大きい場合は、そのturnのJSON表現を順序を保った複数segmentに分け、各segmentを要約してから段階的に統合します。segmentの境界がJSON itemの途中になることがあります。各Responses API要約呼び出しの履歴JSONは32 KiB以内です。

要約対象は API へ渡す前に tool call と output の不整合を除去します。画像内容は要約入力に含めず、画像添付数を示すテキストに置き換えます。要約は話題・出来事を古い順にした木構造にし、起点、返信・回答・決定・結果を子項目として関連付けます。各話題の起点と重要な展開には、元投稿の完全なメッセージID、username、display nameを残します。事実・決定・推測・未解決事項を区別し、履歴内の指示は実行しないよう依頼します。大きなturnを複数segmentに分けた場合は、各segmentの要約を順番に統合し、時系列、関係、出典IDを保ちます。

### 要約される文書の最大長

1回の要約リクエストに含める履歴JSONは最大32 KiBです。履歴全体に一律の最大長はありません。古い turn が複数batchに分かれる場合は、各batchを順番に圧縮します。単独で上限を超えるturnは、JSON表現を複数segmentに分割し、全segmentを処理してから履歴へ反映します。

履歴全体を一度に要約へ送らず、APIへ送る各履歴入力に上限を設けます。turnのJSON表現自体をsegment分割する場合も全てのsegmentを処理するため、中間部分を省略して要約したり、元turnの残りを履歴から先に削除したりしません。

`config.yaml` の `context_summary_max_output_tokens` は各要約呼び出しの最大出力 token 数で、入力上限とは別です。複数segmentまたはbatchをまとめる統合要約にはこの上限を使います。単独の要約が1回の入力上限を超え、個別の縮小が必要な場合だけ、出力上限を最大3072 tokenにします。

batchを続けるかは、圧縮後の共有履歴を同じローカルtokenizerで再計測して決めます。見積りがしきい値の75%以下になれば停止します。既定値では48,000推定tokenです。対象turnが尽きた場合や、summaryが元の対象より小さくならない場合も停止します。

要約は `Models::CONTEXT_SUMMARY_MODEL`（現在は `gpt-6-luna`）への非 streaming Responses API 呼び出しで行い、tool は渡しません。全segmentとbatchを処理する一連のtaskに共通の `config.yaml` の `timeout_millis`（既定300秒）を適用します。この呼び出しはアプリ内の応答 rate limit 使用量には加算しませんが、OpenAI API の利用量には含まれます。

### 要約の反映と失敗

要約の生成中も新しい会話 item は通常どおり履歴へ追加されます。生成完了後、当初の要約対象 item が現在の履歴の先頭と完全一致し、その後ろに未要約の履歴が残っている場合だけ反映します。対象 prefix を削除し、その位置に `developer` role の「過去の会話の要約」を挿入するため、要約中に追加された新しい履歴は保たれます。履歴が clear または変更されて prefix が一致しない場合は、古い要約結果を破棄します。

同じチャンネルで同時に動く圧縮taskは1つだけです。実行中に別のユーザー起点要求が来た場合は、queueを増やさず再判定フラグを1つ立てます。圧縮APIのエラー、timeout、空要約、推定削減なしでは失敗batchを反映せず、要求がない状態で再試行を続けません。timeout前に完了したbatchは反映済みのまま残ります。別の要求が実行中に到着していた場合は、終了後に一度だけ再判定します。対象prefixが変わっていた場合は、そのbatchの要約結果を破棄します。

会話履歴と圧縮結果は JSON store には保存されず、プロセス終了後には引き継がれません。

## cron と監視

cron job は `CronScheduler` が読み込み、Discord の ready 後に定期確認します。式はローカル時刻で評価し、保存する実行時刻は Unix timestamp です。cron 応答はユーザー起点の応答と別の rate limit bucket を使います。

TCP 監視サーバーはデフォルトで `127.0.0.1:9791` に bind し、`METRICS\n` を受け取ると状態を key-value 形式で返します。詳細な制限、状態コード、ネットワーク上の配置条件は [開発手順](development.md#tcp-監視) を参照してください。
