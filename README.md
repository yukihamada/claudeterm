# claudeterm

超高速 Claude Code ウェブターミナル。ブラウザから誰でも使える。

## ローカル起動

```bash
cargo run
# → http://localhost:3000
```

## 環境変数

| 変数 | 説明 | デフォルト |
|------|------|-----------|
| `PORT` | ポート番号 | `3000` |
| `AUTH_TOKEN` | アクセストークン（未設定=認証なし） | なし |
| `CLAUDE_COMMAND` | 起動コマンド | `claude` |

### 認証付きで起動

```bash
AUTH_TOKEN=mysecret cargo run
# → http://localhost:3000/?token=mysecret
```

## Fly.io デプロイ

```bash
fly launch --name claudeterm
fly secrets set AUTH_TOKEN=<your-token>
fly deploy --remote-only
```

## アーキテクチャ

```
Browser ←──── WebSocket ────→ Axum (Rust)
  xterm.js (binary frames)        │
                               portable-pty
                                   │
                               claude CLI
```
