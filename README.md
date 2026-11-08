<div align="center">
<h1>Nelfie</h1>
</div>

Nelfie(ネルフィー) は、Discord 上で会話・ツール実行・VOICEVOX 読み上げを統合して動かす Rust 製の bot です。

Observer-rust の後継プロジェクトとして、スタンドアローン動作を前提に設計されており、OpenAI API を利用した会話機能を中心に、様々なツールや機能を提供します。

## 機能
- 会話: OpenAI API を利用した会話機能 モデル選択可能 レート制限可能 システムプロンプト設定可能
  - tools:
    - get_time.rs: 国コードから現在の時刻を取得
    - latex.rs: TeX 数式を画像化
    - modal_builder.rs: ストレージなしの短い stateless trigger でモーダルダイアログを動的に構築
    - discord.rs: Discord 操作系ツール（例: メッセージ送信、回答者限定の ephemeral 応答、チャンネル管理）
    - voicevox.rs: VOICEVOX 操作系ツール（例: 話者変更、スタイル変更、読み上げ）
- VOICEVOX 読み上げ: VC内でのテキスト読み上げ機能 話者・スタイル選択可能 自動読み上げ機能
- TeX 数式の画像化: TeX 数式を画像化して Discord に送信する機能

## システム要件
- OS: Windows 10/11 (64-bit), Linux (x86_64)
- RAM: 3GB以上（ページアウトを期待できるなら2GBでも動作可能）
- CPU: x86_64-v3(AVX2をサポートしていなければなりません)
- ストレージ: 4GB以上の空き容量（VOICEVOXモデルとONNX Runtimeを含む）

ONNX Runtime の HWアクセラレーション については[voicevox_onnxruntime](https://github.com/VOICEVOX/onnxruntime-builder/releases)のリリースノートを参照してください。

## セットアップ

1. リリースからバイナリをダウンロード

2. `.env` を作成

```dotenv
# required
DISCORD_TOKEN=xxxxxxxxxxxxxxxx
OPENAI_API_KEY=sk-xxxxxxxxxxxxxxxx

# 以下optional
SYSTEM_PROMPT=あなたのシステムプロンプト
CONTEXT_COMPACTION_TOKEN_LIMIT=64000
CONTEXT_SUMMARY_MAX_OUTPUT_TOKENS=4096
# 管理者専用コマンド用。Discord user IDをカンマ区切りで指定（未設定なら管理者なし）
# ADMIN_USER_IDS=123456789012345678,234567890123456789
# 詳細ログが必要な場合のみ指定（既定は warn,nelfie=info）
# RUST_LOG=warn,nelfie=debug
VOICEVOX_DEFAULT_SPEAKER=3
VOICEVOX_CORE_ACCELERATION=auto
VOICEVOX_CORE_CPU_THREADS=0
VOICEVOX_OUTPUT_SAMPLING_RATE=48000 # 24000 の倍数であるべきです
VOICEVOX_OPEN_JTALK_DICT_DIR=voicevox_core/dict/open_jtalk_dic_utf_8-1.11
VOICEVOX_VVM_DIR=voicevox_core/models/vvms
# 実行時リンク(load-onnxruntime)では voicevox_onnxruntime を自動探索してロードします
# 通常は未設定でOK。明示パス/ファイル名を指定したい場合のみ設定してください
VOICEVOX_ONNXRUNTIME_FILENAME=
MONITOR_TCP_BIND=127.0.0.1:9791
```

3. 起動

起動時に VOICEVOX core を初期化してから Discord Gateway を開始します。必要な辞書・モデル・ONNX Runtime が配置されていない場合は取得するため、初回起動にはネット接続と時間が必要です。


## 設定

現在の実装では、環境変数（`.env`）から読み込みます。
`DISCORD_TOKEN` と `OPENAI_API_KEY` は未設定だと起動時にエラーになります。
`ADMIN_USER_IDS` は管理者専用の `/rate config` と `/set_system_prompt` に使う数値の Discord user ID のカンマ区切り一覧です。未設定または空の場合、管理者専用コマンドを実行できるユーザーはいません。数値以外を含めると起動時にエラーになります。

ログは既定でNelfie自身を`INFO`、serenityなどの依存ライブラリを`WARN`以上に絞ります。端末では`INFO`や`WARN`などのレベルを自動で色分けし、`RUST_LOG_STYLE=always`または`never`で明示できます。モデル応答はANSIエスケープ列と制御文字を除去して1行で記録します。詳細調査時は`RUST_LOG=warn,nelfie=debug`、依存ライブラリも含める場合は標準の`RUST_LOG`フィルタ構文で対象モジュールを追加してください。

現行実装ではVOICEVOX coreを起動時に初期化し、選択されたVVMをすべて読み込みます。
VVM の更新では新しいファイルを一時ディレクトリへ先に取得します。取得に失敗した場合や、入れ替え失敗後に旧 VVM を復元できた場合は、それを使って起動を続けます。復元にも失敗した場合と初回取得に失敗した場合は起動エラーになります。

## TCP監視

身内用機能です。

Nelfieは`MONITOR_TCP_BIND`で指定したアドレスにTCP監視サーバーを起動します。既定は`127.0.0.1:9791`です。監視サーバーから接続し、`METRICS\n`を送ると、key-value形式のメトリクスを返します。応答は空行で終わります。

`192.168.0.254`で待ち受ける場合は、Nelfie側の`.env`にも`MONITOR_TCP_BIND=192.168.0.254:9791`を設定してください。

```yaml
- id: "discord-bot.nelfie"
  name: "nelfie"
  type: tcp
  host: "192.168.0.254"
  port: 9791
  interval: 60
  timeout: 5
  slow_ms: 500
  hide_target: true
  exclude_status: false
  send: "METRICS\n"
  response_end: "\n\n"
  order: 200
  max_bytes: 16384
  response_codes:
    0: operational
    1: degraded
    2: down
```

## 主なコマンド

この bot は slash command と prefix command（`!`）の両方を登録します。

BASIC:

- `/ping`: Discord API との遅延を測定して返す
- `/status`: Nelfieのバージョンと実行環境を表示
- `/tex <expr>`: TeX 数式を画像化して送信

CHAT BOT:

- `/enable`: ChatBot 機能を有効化
- `/disable`: ChatBot 機能を無効化
- `/clear`: 会話履歴をクリア
- `/model`: 使用する OpenAI モデルを選択
- `/rate config <target_user> <limit>`: レート制限の設定(管理者のみ)
- `/rate status [target_user]`: 現在のモデルコスト、一般/cronバケットの `rate_line`、残量、毎時回復量を表示
- `/set_system_prompt`: システムプロンプトの設定(管理者のみ)
- `/cron add <schedule> <prompt>`: 実行したチャンネルに、cron 表記で定期実行プロンプトを登録
- `/cron test [id]`: 登録済み cron を時刻に関係なく即時実行。IDを省略できるのは、このチャンネルに登録が1件だけの場合です。
- `/cron delete <id>`: 登録済み cron を削除

VC / TTS:

- `/vc join [auto_read]`: ボイスチャンネルに参加します。オプションでこのテキストチャンネルの自動読み上げを有効化できます。
- `/vc leave`: ボイスチャンネルから退出します。
- `/vc say <text>`: 指定したテキストを読み上げます。
- `/vc download <text>`: 現在の話者設定でWAV音声を生成し、ダウンロード可能なファイルとして送信します。
- `/vc config [system_read] [auto_read] [parallel_count]`: システム読み上げ、自動読み上げ、並列数を更新します。省略した項目は現在の設定を維持します。
- `/vc autoread <enabled>`: このテキストチャンネルでの自動読み上げの有効/無効を切り替えます。
- `/vc dict <source> <target>`: このテキストチャンネルの読み上げ辞書を登録または更新します。
- `/vc dict-list`: このテキストチャンネルの読み上げ辞書を登録者・最終更新者とともに10件ずつ表示します。ボタンでページを切り替えられます。
- `/vc dict-delete <source>`: このテキストチャンネルの読み上げ辞書エントリを削除します。
- `/vc dict-user <source> <target>`: 自分用の読み上げ辞書を登録または更新します。
- `/vc dict-user-list`: 自分用の読み上げ辞書を登録者・最終更新者とともに10件ずつ表示します。ボタンでページを切り替えられ、スラッシュコマンドでは本人にだけ表示します。
- `/vc dict-user-delete <source>`: 自分用の読み上げ辞書エントリを削除します。
- `/vc speaker <speaker> <style> [speed] [pitch] [pan]`: 自分の話者、スタイル、話速、音高、左右パンを設定します。
- `/vc status`: 現在の VC 状態と VOICEVOX 設定を表示します。

cron は実行環境のローカル時刻で、標準的な 5 フィールド表記（例: `*/30 * * * *`）を使います。prefix command では `cron add "*/30 * * * *" <prompt>` のように cron 表記を引用してください。AI も `cron-tool` で cron の作成・一覧・削除を行えます。cron 実行は通常応答とは別の guild 単位 cron バケットで制御され、標準設定では 1 guild あたりおおむね 1 時間に 1 回実行できます。通常の `/rate config` は一般応答用のレート制限だけを変更します。

利用可能なモデルとレート係数は `src/llm/models.rs` で定義します。現在の選択肢は `gpt-5.4-mini` (x3)、`gpt-5.4-nano` (x1)、`gpt-5.6-luna` (x3)、`gpt-5.6-terra` (x20)、`gpt-6-luna` (x1)、`gpt-6-sol` (x20)、`gpt-6.1-sol` (x20)、`o4-mini` (x3) です。アプリのレート制限は実際のtoken数ではなく、モデルごとの固定コスト係数を使います。実際のAPI請求額はtoken数や長文context向け価格によって変わります。モデル一覧と現在モデルでは、レート制限で消費するコストを `xN` 形式で表示します。ユーザー既定、システム、cron、context要約のモデルは `Models` の `USER_DEFAULT_MODEL`、`SYSTEM_MODEL`、`CRON_MODEL`、`CONTEXT_SUMMARY_MODEL` で用途別に定義します。現在はいずれも `gpt-6-luna` です。

`model-control` ツールで mainline Responses モデルと画像生成モデルを切り替えられます。画像生成モデルの初期値は `gpt-image-2.5-flare` です。画像生成前にモデル設定を読み取るか変更し、次のResponses APIラウンドで設定済みモデルを使うネイティブ `image_generation` ツールを有効にします。推論レベルは常に `low` です。画像生成の開始1回ごとに追加でレート枠を1単位消費します。スラッシュコマンドでは `/model get` と `/model image-set` で確認・変更できます。

通常応答はチャンネルごとに安定した`prompt_cache_key`を送信して、同じ会話contextのprefix cacheを再利用します。5.6系モデルではさらに`prompt_cache_retention=24h`を明示してextended prompt cacheを有効化します。GPT-6系モデルはAPIの標準cache制御を使います。APIが返す`cached_tokens`はtool-callを含む全ラウンドで集計し、応答完了ログへ`prompt_cache=hit`または`prompt_cache=miss`とともに表示します。

会話履歴は固定件数のローリングウィンドウでは削除しません。応答要求がレート制限を通過した時点で、`tiktoken-rs` の `o200k_base` による見積りが `CONTEXT_COMPACTION_TOKEN_LIMIT` (既定64,000) 以上なら、古いturnの要約をバックグラウンドで開始します。応答APIには履歴全体を送らず、同じ見積りでしきい値の75% (既定48,000) に収まる直近turnを使います。GPT-6用 tokenizer の対応表はないため、この値はAPIの実測token数ではなく、画像・要求定義用の余裕を含めた推定です。要約入力の上限や失敗時の扱いは[コンテキスト圧縮の仕様](docs/architecture.md#会話履歴のコンテキスト圧縮)を参照してください。メンションのない発言から応答・圧縮APIは呼びません。

## Discord 添付画像URLの更新

Discordの添付ファイルURLは署名付きで、クエリパラメータの `ex` は有効期限の16進Unix timestamp、`is` は発行時刻、`hm` は期限まで有効な署名です。詳細は[Discord公式のSigned Attachment CDN URLs](https://docs.discord.com/developers/reference#signed-attachment-cdn-urls)を参照してください。

Nelfieは画像バイナリをBOT側でダウンロードせず、URLと元メッセージ/添付IDだけを会話履歴に保持します。OpenAI APIへ送信する直前に `ex` を確認し、有効期限の10分前を過ぎていればDiscordのメッセージを再取得してURLを更新します。`ex` が存在しない、または解析できないURLでは、取得から1時間後に更新するフォールバックを使います。

DiscordはTTLの固定値を公式には保証していません。Discord添付バイナリURLのTTLは、現時点では最短24時間、最長14日程度と推測されています。公式ドキュメントの例は `is` から `ex` までちょうど14日ですが、実装はこの推測値に依存せず、各URLの `ex` を基準にします。

## 開発者向けドキュメント

コード構成、開発手順、VOICEVOX の処理設計は [docs/README.md](docs/README.md) から参照できます。

## トラブルシュート

- `DISCORD_TOKEN must be set`:
	`.env` に `DISCORD_TOKEN` を設定してください。
- `OPENAI_API_KEY must be set`:
	`.env` に `OPENAI_API_KEY` を設定してください。
- VOICEVOX 関連で辞書/モデル/onnxruntime が見つからない:
	`.env` の VOICEVOX 系パス、または `voicevox_core` 配下のディレクトリ構成を確認してください。

## Third-party licenses

- KaTeX font と Noto Serif JP のライセンスは [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) に記載しています。
- `fonts` ディレクトリを再配布する場合は、上記ライセンス表記を同梱してください。

ダウンロードコンテンツはREADMEを含むのでそちらを参照。
