# 開発手順

## ローカル実行

Rust 2024 edition に対応した Rust toolchain を使います。必要な環境変数、OS 要件、VOICEVOX の配置と初回起動の動作はリポジトリ直下の [README.md](../README.md) を参照してください。

リポジトリのルートで `.env` を用意し、必須の `DISCORD_TOKEN` と `OPENAI_API_KEY` を設定してから実行します。初回起動時に `config.yaml` が自動生成され、非秘密設定はそこから読み込まれます。管理者専用コマンドを使う場合は `admin_users` に数値の Discord user ID を YAML のリスト形式で指定します。未設定または空なら管理者専用コマンドは誰にも許可されません。

```powershell
cargo run --locked
```

## チェック

変更内容に応じて次を実行します。

```powershell
cargo fmt --all -- --check
cargo check --locked
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

これらはコンパイルや Rust 内の振る舞いを確認します。Discord Gateway、OpenAI API、GitHub からの VOICEVOX asset download、ONNX Runtime の実音声合成、Songbird 再生は、それぞれ実際の認証情報・ネットワーク・実行環境を使った確認が別に必要です。

## 変更箇所の目安

- Discord コマンドは `src/discord/commands/` に実装し、`src/discord/command_registry.rs` へ登録します。
- Discord Gateway event の振り分けは `src/discord/events.rs` にあります。処理本体は `message_events.rs`、`interactions.rs`、`voice_events.rs` などに分かれています。
- Responses API tool は `src/llm/tools/` に `LMTool` として実装し、`src/llm/tools/mod.rs` の registry に登録します。
- 新しい状態は、チャンネル・ユーザー・guild 接続など適切な所有者へ置きます。再起動後に残す必要がある場合は、対応する JSON store の読み込み・保存形式も確認します。
- LLM 応答の制御と Discord 配送は `src/discord/responses.rs`、Responses API と tool round は `src/llm/client.rs` が入口です。

ログの既定値は Nelfie が `INFO`、依存ライブラリが `WARN` 以上です。詳細ログには次を使えます。

```dotenv
RUST_LOG=warn,nelfie=debug
```

モデル応答や Gateway の問題を調べるときは、まず `component`、`request_id`、`channel` などの識別情報を含む Nelfie のログを確認します。API token などの secret はログやサンプル設定へ出力しないでください。

## TCP 監視

`TcpMonitorServer` は `config.yaml` の `monitor_tcp_bind` で指定されたアドレスに bind します。既定値は `127.0.0.1:9791` です。接続後に `METRICS\n` を送ると、空行で終端する key-value 応答が返ります。

- 受信要求は最大 64 bytes、読み取り timeout は 2 秒です。
- 同時クライアントは最大 16 接続です。上限を超えた接続は閉じられます。
- TLS と認証はありません。既定の loopback bind を維持するか、信頼できるネットワーク内だけで公開してください。
- `status.code` は `0` が VOICEVOX と Gateway の両方が ready、`1` が VOICEVOX ready の状態で Gateway 再接続中、`2` が VOICEVOX 未準備または Gateway 未接続です。
- `discord.gateway.heartbeat_rtt.max` と `response.rtt` は Gateway の heartbeat ACK が得られた場合だけ出力されます。Nelfie は追加の probe を送りません。

監視サーバーは VOICEVOX 初期化と Discord Gateway 接続より先に起動します。そのため初期化中は接続できても、準備状態が `2` の場合があります。プロトコルのリクエスト処理・接続上限は `src/app/tcp_monitor.rs`、状態の集計は `src/app/monitoring.rs` にあります。
