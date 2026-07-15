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
# 詳細ログが必要な場合のみ指定（既定は warn,nelfie=info）
# RUST_LOG=warn,nelfie=debug
VOICEVOX_DEFAULT_SPEAKER=3
VOICEVOX_CORE_ACCELERATION=auto
VOICEVOX_CORE_CPU_THREADS=0
VOICEVOX_CORE_LOAD_ALL_MODELS=false
VOICEVOX_OUTPUT_SAMPLING_RATE=48000 # 24000 の倍数であるべきです
VOICEVOX_PRELOAD_ON_STARTUP=true
VOICEVOX_OPEN_JTALK_DICT_DIR=voicevox_core/dict/open_jtalk_dic_utf_8-1.11
VOICEVOX_VVM_DIR=voicevox_core/models/vvms
# 実行時リンク(load-onnxruntime)では voicevox_onnxruntime を自動探索してロードします
# 通常は未設定でOK。明示パス/ファイル名を指定したい場合のみ設定してください
VOICEVOX_ONNXRUNTIME_FILENAME=
```

3. 起動

v0.1.1 以前はVOICEVOXの初期化がlazyなため使用時に数分間の待ち時間があります。  
初期化終了後は次回起動以降も高速に起動します。　　
v0.1.4以降はserenityの起動とdownloaderの起動が直列化しました。


## 設定

現在の実装では、環境変数（`.env`）から読み込みます。
`DISCORD_TOKEN` と `OPENAI_API_KEY` は未設定だと起動時にエラーになります。

ログは既定でNelfie自身を`INFO`、serenityなどの依存ライブラリを`WARN`以上に絞ります。端末では`INFO`や`WARN`などのレベルを自動で色分けし、`RUST_LOG_STYLE=always`または`never`で明示できます。モデル応答はANSIエスケープ列と制御文字を除去して1行で記録します。詳細調査時は`RUST_LOG=warn,nelfie=debug`、依存ライブラリも含める場合は標準の`RUST_LOG`フィルタ構文で対象モジュールを追加してください。

## 主なコマンド

この bot は slash command と prefix command（`!`）の両方を登録します。

BASIC:

- `/ping`: Discord API との遅延を測定して返す
- `/tex_expr`: TeX 数式を画像化して送信

CHAT BOT:

- `/enable`: ChatBot 機能を有効化
- `/disable`: ChatBot 機能を無効化
- `/clear`: 会話履歴をクリア
- `/model`: 使用する OpenAI モデルを選択
- `/rate_config`: レート制限の設定(管理者のみ)
- `/rate_status [target_user]`: 現在のモデルコスト、一般/cronバケットの `rate_line`、残量、毎時回復量を表示
- `/set_system_prompt`: システムプロンプトの設定(管理者のみ)
- `/cron <cron> <prompt>`: 実行したチャンネルに、cron 表記で定期実行プロンプトを登録
- `/cron_test [id]`: 登録済み cron を時刻に関係なく即時実行
- `/del_cron <id>`: 登録済み cron を削除

VC / TTS:

- `/vc_join [auto_read]`: ボイスチャンネルに参加します。オプションで自動読み上げを有効化できます。
- `/vc_leave`: ボイスチャンネルから退出します。
- `/vc_say <text>`: 指定したテキストを読み上げます。
- `/vc_download <text>`: 現在の設定でWAV音声を生成し、ダウンロード可能なファイルとして送信します。
- `/vc_autoread <enabled>`: 自動読み上げの有効/無効を切り替えます。
- `/vc_dict <source> <target>`: 読み上げの辞書エントリを追加/削除します。
- `/vc_speaker ...`: 話者, スタイル, 音程, 速さ, パンの設定を行います。
- `/vc_status`: 現在の VC 状態と VOICEVOX 設定を表示します。
- `/vc_config ...`: 読み上げの詳細設定を行います。(自動読み上げ, システム読み上げ, 並列読み上げ)

cron は実行環境のローカル時刻で、標準的な 5 フィールド表記（例: `*/30 * * * *`）を使います。prefix command では cron 表記を `"*/30 * * * *"` のように引用してください。AI も `cron-tool` で cron の作成・一覧・削除を行えます。cron 実行は通常応答とは別の guild 単位 cron バケットで制御され、標準設定では 1 guild あたりおおむね 1 時間に 1 回実行できます。通常の `/rate_config` は一般応答用のレート制限だけを変更します。

利用可能なモデルには `gpt-5.6-luna`（cost x3）、`gpt-5.6-terra`（cost x6）、`gpt-5.6-sol`（cost x9）を含みます。モデル一覧と現在モデルでは、レート制限で消費するコストを `xN` 形式で表示します。ユーザー既定、システム、cron、context要約のモデルは `Models` の `USER_DEFAULT_MODEL`、`SYSTEM_MODEL`、`CRON_MODEL`、`CONTEXT_SUMMARY_MODEL` で用途別に定義します。現在はいずれも `gpt-5.6-luna` です。

通常応答はチャンネルごとに安定した`prompt_cache_key`を送信して、同じ会話contextのprefix cacheを再利用します。5.6系モデルではさらに`prompt_cache_retention=24h`を明示してextended prompt cacheを有効化します。APIが返す`cached_tokens`はtool-callを含む全ラウンドで集計し、応答完了ログへ`prompt_cache=hit`または`prompt_cache=miss`とともに表示します。

会話履歴は固定件数のローリングウィンドウでは削除しません。ユーザー起点のAI応答が成功した時だけ、OpenAI APIが返す `usage.total_tokens` を確認し、`CONTEXT_COMPACTION_TOKEN_LIMIT` 以上なら古い側のおよそ半分を `Models::CONTEXT_SUMMARY_MODEL` で非同期要約します。通常メッセージの流入やcron実行だけでは要約を開始しません。要約中に追加された新しい履歴は残し、同じチャンネルでは複数の要約を同時実行しません。要約の最大出力token数は `CONTEXT_SUMMARY_MAX_OUTPUT_TOKENS` で設定します。

## Discord 添付画像URLの更新

Discordの添付ファイルURLは署名付きで、クエリパラメータの `ex` は有効期限の16進Unix timestamp、`is` は発行時刻、`hm` は期限まで有効な署名です。詳細は[Discord公式のSigned Attachment CDN URLs](https://docs.discord.com/developers/reference#signed-attachment-cdn-urls)を参照してください。

Nelfieは画像バイナリをBOT側でダウンロードせず、URLと元メッセージ/添付IDだけを会話履歴に保持します。OpenAI APIへ送信する直前に `ex` を確認し、有効期限の10分前を過ぎていればDiscordのメッセージを再取得してURLを更新します。`ex` が存在しない、または解析できないURLでは、取得から1時間後に更新するフォールバックを使います。

DiscordはTTLの固定値を公式には保証していません。Discord添付バイナリURLのTTLは、現時点では最短24時間、最長14日程度と推測されています。公式ドキュメントの例は `is` から `ex` までちょうど14日ですが、実装はこの推測値に依存せず、各URLの `ex` を基準にします。

## 開発用チェック

```powershell
cargo clippy --all-targets --all-features -- -D warnings
```

fmtはやってない。

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
