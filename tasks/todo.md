# claudeterm 3機能実装計画

## 現状分析

### DBスキーマ (init_db より)

```sql
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY, email TEXT UNIQUE, token TEXT UNIQUE,
    credits REAL DEFAULT 10.0, api_key TEXT, created_at TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY, user_id TEXT, name TEXT, created_at TEXT, project TEXT,
    claude_sid TEXT,
    FOREIGN KEY(user_id) REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT, role TEXT, content TEXT, timestamp TEXT
);
CREATE TABLE IF NOT EXISTS otps (
    email TEXT PRIMARY KEY, code TEXT NOT NULL, expires_at INTEGER NOT NULL
);
```

### 現行ルート一覧 (main.rs)

| Method | Path | Handler |
|--------|------|---------|
| GET | / | index.html |
| GET | /health | ok |
| GET | /manifest.json | manifest |
| POST | /api/auth/login | login (OTP送信) |
| POST | /api/auth/verify | verify_otp |
| GET | /api/auth/google | google_oauth_start |
| GET | /auth/google/callback | google_oauth_callback |
| GET | /api/auth/google/callback | google_oauth_callback |
| GET | /api/auth/local-login | local_login |
| GET | /api/auth/me | me |
| POST | /api/auth/apikey | set_api_key |
| GET | /api/sessions | list_sessions |
| POST | /api/sessions | create_session |
| DELETE | /api/sessions/:id | delete_session |
| GET | /api/sessions/:id/messages | get_messages |
| GET | /api/files | list_files |
| GET | /api/files/read | read_file |
| GET | /api/projects | list_projects |
| GET | /api/templates | list_templates |
| POST | /api/billing/checkout | create_checkout |
| POST | /api/billing/webhook | stripe_webhook |
| GET | /billing/success | billing_success |
| POST | /api/image | generate_image |
| POST | /api/admin/alert | admin_alert |
| GET | /ws | ws_handler |

### AppState 構造体

```rust
struct AppState {
    command: String,          // CLAUDE_COMMAND env (default: "claude")
    workdir: String,          // WORKDIR env (default: /tmp/claudeterm-sandbox)
    db: Db,                   // Arc<StdMutex<Connection>>
    admin_token: Option<String>, // AUTH_TOKEN env
    stripe_key: Option<String>,  // STRIPE_SECRET_KEY env
    resend_key: Option<String>,  // RESEND_API_KEY env
    gemini_key: Option<String>,  // GEMINI_API_KEY env
    base_url: String,         // BASE_URL env
    limiter: Arc<billing::RateLimiter>,
    active_procs: Arc<StdMutex<HashMap<String, bool>>>,
}
```

### フロントエンド構造 (index.html, 740行)

- 単一HTMLファイル、`include_str!()` でバイナリに埋め込み
- インライン CSS + JS (minified スタイル)
- WebSocket で claude CLI とリアルタイム通信
- ルーティング: ログイン画面 (#login) / アプリ本体 (#app)
- セッションタブ、ファイルサイドバー、設定モーダル、Cmd-K パレット
- 既存モーダルパターン: `#settings` (display:none → .on で表示)

---

## 実装する3機能

---

## 機能1: 静的サイト 1クリックデプロイ (Cloudflare Pages / Fly.io)

### 概要
ユーザーのサンドボックス内で `index.html` / `dist/` / `public/` を検出し、
Cloudflare Pages API または `fly launch` でデプロイする。
デプロイ後に `<project>.chatweb.ai` サブドメインを付与する。

### DBマイグレーション (新テーブル)

```sql
CREATE TABLE IF NOT EXISTS deployments (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    project TEXT NOT NULL,       -- ユーザーサンドボックス内ディレクトリ名
    provider TEXT NOT NULL,      -- "cloudflare" | "fly"
    deploy_url TEXT,             -- デプロイ後のURL
    subdomain TEXT,              -- <subdomain>.chatweb.ai
    status TEXT DEFAULT 'pending', -- pending | deploying | live | error
    last_deployed_at TEXT,
    error TEXT,
    FOREIGN KEY(user_id) REFERENCES users(id)
);
```

### 新規ルート

| Method | Path | 役割 |
|--------|------|------|
| GET | /api/deploy/detect | ワークスペース内の静的成果物を検出 |
| POST | /api/deploy | デプロイ開始 (provider, project を受け取る) |
| GET | /api/deploy | デプロイ一覧取得 |
| GET | /api/deploy/:id | デプロイ状態ポーリング |

### 実装ステップ

- [ ] Step 1: `src/deploy.rs` を新規作成 (推定: 中)
  - `detect_artifacts(user_sandbox: &str, project: &str) -> Vec<ArtifactType>`
    - `index.html`, `dist/`, `public/`, `out/` を検出
  - `deploy_cloudflare_pages(api_token, account_id, project_name, path) -> Result<String>`
    - Cloudflare Pages Direct Upload API (`POST /accounts/{id}/pages/projects/{name}/deployments`)
    - ファイルツリーをマルチパートでアップロード
  - `deploy_fly_static(project_name, path) -> Result<String>`
    - `fly launch --no-deploy` で fly.toml 生成
    - `fly deploy --remote-only` を tokio::process::Command で非同期実行

- [ ] Step 2: `main.rs` にルート追加 (推定: 小)
  - AppState に `cf_api_token: Option<String>`, `cf_account_id: Option<String>` を追加
  - 環境変数: `CF_API_TOKEN`, `CF_ACCOUNT_ID`
  - `deployments` テーブルを init_db に追加

- [ ] Step 3: フロントエンド (推定: 中)
  - ファイルサイドバー (`#sb`) に「Deploy」ボタン追加
  - デプロイ選択モーダル: Cloudflare Pages / Fly.io の2択
  - `GET /api/deploy/detect` でアーティファクト検出 → ファイル一覧表示
  - デプロイ中はプログレス表示 (ポーリング or WebSocket メッセージ活用)
  - デプロイ完了後に URL をチャットに表示

### 注意点
- Cloudflare Pages は最大 20,000ファイル制限 → 大きい `dist/` のみ対象
- `fly` CLI がコンテナに入っていることが前提。なければ Cloudflare のみ提供
- サブドメイン付与は Cloudflare DNS API で CNAME レコードを追加

---

## 機能2: Secrets ボールト

### 概要
ユーザーが登録したシークレット (key=value) を AES-256-GCM で暗号化して SQLite に保存。
Claude CLI の PTY 起動時に環境変数として注入する。

### DBマイグレーション

```sql
CREATE TABLE IF NOT EXISTS secrets (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    key_name TEXT NOT NULL,
    encrypted_value TEXT NOT NULL,  -- AES-256-GCM, base64エンコード
    created_at TEXT,
    updated_at TEXT,
    UNIQUE(user_id, key_name),
    FOREIGN KEY(user_id) REFERENCES users(id)
);
```

### 暗号化設計
- マスターキー: `SECRET_VAULT_KEY` 環境変数 (32バイト hex) / 未設定時は XOR+base64 の簡易実装で fallback
- 実装: `aes-gcm` クレート (Cargo.toml に追加)
  - `aes-gcm = "0.10"`
- ノンス: 12バイト random (暗号文にプレフィクスとして付与)
- 保存形式: `nonce_base64:ciphertext_base64`

### 新規ルート

| Method | Path | 役割 |
|--------|------|------|
| GET | /api/secrets | シークレット一覧 (key名のみ、値は返さない) |
| POST | /api/secrets | シークレット追加/更新 |
| DELETE | /api/secrets/:key | シークレット削除 |

### Claude CLI への注入
`main.rs` の `handle_ws` 内、claude コマンド起動直前:

```rust
// secrets を復号して cmd.env() で注入
let user_secrets = load_and_decrypt_secrets(&state.db, &uid);
for (k, v) in user_secrets {
    cmd.env(&k, &v);
}
```

### 実装ステップ

- [ ] Step 1: `src/vault.rs` を新規作成 (推定: 中)
  - `encrypt(master_key: &[u8], plaintext: &str) -> String`
  - `decrypt(master_key: &[u8], encoded: &str) -> Option<String>`
  - `load_and_decrypt_secrets(db: &Db, user_id: &str) -> Vec<(String, String)>`

- [ ] Step 2: Cargo.toml に依存追加 (推定: 小)
  - `aes-gcm = "0.10"`
  - `rand = "0.8"` (既に存在)

- [ ] Step 3: `main.rs` に CRUD ルート追加 (推定: 小)
  - `secrets` テーブルを init_db に追加
  - AppState に `vault_key: Vec<u8>` を追加 (SECRET_VAULT_KEY env から派生)
  - handle_ws の claude 起動直前に注入コード追加

- [ ] Step 4: フロントエンド UI (推定: 中)
  - Settings モーダル (`#settings` `.stw`) に「Secrets」セクションを追加
  - `GET /api/secrets` でキー名一覧を取得・表示
  - キー名 + 値のペアを入力する行を追加/削除
  - 値フィールドは `type="password"` で隠蔽
  - 保存ボタンで `POST /api/secrets` を呼ぶ

---

## 機能3: チームワークスペース

### 概要
チームを作成し、招待リンクでメンバーを追加。
チームのシークレット vault を共有、将来的にはワークスペースも共有可能にする。

### DBマイグレーション

```sql
CREATE TABLE IF NOT EXISTS teams (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    invite_token TEXT UNIQUE,    -- 招待リンクトークン
    created_at TEXT,
    FOREIGN KEY(owner_id) REFERENCES users(id)
);

CREATE TABLE IF NOT EXISTS team_members (
    team_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT DEFAULT 'member',  -- 'owner' | 'member'
    joined_at TEXT,
    PRIMARY KEY(team_id, user_id),
    FOREIGN KEY(team_id) REFERENCES teams(id),
    FOREIGN KEY(user_id) REFERENCES users(id)
);

-- secrets テーブルに team_id カラムを追加 (チーム共有シークレット)
ALTER TABLE secrets ADD COLUMN team_id TEXT REFERENCES teams(id);
-- user_id が NULL のとき team_id で引く (チーム共有)
-- user_id が非NULL のとき個人シークレット
```

### 新規ルート

| Method | Path | 役割 |
|--------|------|------|
| GET | /api/teams | 自分が所属するチーム一覧 |
| POST | /api/teams | チーム作成 |
| DELETE | /api/teams/:id | チーム削除 (owner のみ) |
| POST | /api/teams/:id/invite | 招待トークン生成/再生成 |
| GET | /api/teams/join/:token | 招待リンク受け入れ |
| GET | /api/teams/:id/members | メンバー一覧 |
| DELETE | /api/teams/:id/members/:uid | メンバー除名 |
| GET | /api/teams/:id/secrets | チーム共有シークレット一覧 |
| POST | /api/teams/:id/secrets | チーム共有シークレット追加/更新 |
| DELETE | /api/teams/:id/secrets/:key | チーム共有シークレット削除 |

### Claude CLI への Secrets 注入 (拡張)
個人シークレットに加えて、所属チームの共有シークレットも注入。
個人 > チームの優先順位で上書き。

### 実装ステップ

- [ ] Step 1: DBマイグレーション (推定: 小)
  - init_db に `teams`, `team_members` テーブル追加
  - `secrets` テーブルに `team_id` 追加の ALTER TABLE (migration 既存パターンに倣う)

- [ ] Step 2: `src/teams.rs` を新規作成 (推定: 中)
  - チーム CRUD ハンドラ群
  - 招待トークン生成: `uuid::Uuid::new_v4()` の短縮 token
  - `GET /api/teams/join/:token` → user を team_members に INSERT

- [ ] Step 3: `main.rs` にルート追加 (推定: 小)
  - `mod teams;` 追加
  - Router に全 teams ルートを追加

- [ ] Step 4: vault.rs 拡張 (推定: 小)
  - `load_and_decrypt_secrets` をチームシークレットも含めるよう拡張

- [ ] Step 5: フロントエンド UI (推定: 大)
  - Settings モーダルに「Team」タブを追加
  - チーム作成フォーム (チーム名)
  - メンバー一覧表示
  - 招待リンクのコピーボタン
  - チーム共有シークレット CRUD (機能2のシークレット UI を再利用)
  - ワークスペース共有 (将来拡張用として UI のみ先行実装)

---

## 実装順序と依存関係

```
機能2 (Secrets vault) → 機能1 (Deploy) に secrets 注入が有用
機能2 (Secrets vault) → 機能3 (Team) の共有 vault の基盤
機能3 (Team)          → 機能2 が完成してから実装

推奨順:
1. 機能2 Step 1-3 (vault.rs + DB + API)
2. 機能2 Step 4 (フロントエンド)
3. 機能1 Step 1-2 (deploy.rs + ルート)
4. 機能1 Step 3 (フロントエンド)
5. 機能3 Step 1-4 (teams.rs + DB + ルート + vault 拡張)
6. 機能3 Step 5 (フロントエンド)
```

---

## フロントエンド変更点まとめ

### 追加するモーダル/UI コンポーネント

1. **Secrets セクション** (Settings モーダル内)
   - 既存 `#settings .stw` に追加するだけで整合
   - `<div id="secrets-section">` を `<div class="row">` の前に挿入

2. **デプロイボタン** (ファイルサイドバー `.sbh` に追加)
   - `<button class="ib" id="deploy-btn">🚀</button>` を `.sbh` ボタン列に追加
   - クリックでデプロイモーダルを開く

3. **デプロイモーダル** (`#deploy-modal`, 既存 `#settings` と同パターン)
   - provider 選択 (Cloudflare / Fly)
   - 検出されたアーティファクト一覧表示
   - デプロイ実行ボタン + ステータス表示

4. **チームタブ** (Settings モーダル内)
   - `<div id="team-section">` を Secrets セクションの後に追加

### CSS 追加
既存の CSS 変数 (`--ac`, `--gn`, `--rd` など) をそのまま流用。
新規クラスは既存の命名規則 (短縮2-3文字クラス) に合わせる。

---

## Cargo.toml への追加依存

```toml
aes-gcm = "0.10"
# rand = "0.8" は既存
```

---

## 環境変数 (新規)

| 変数名 | 用途 | デフォルト |
|--------|------|-----------|
| `SECRET_VAULT_KEY` | AES-256 マスターキー (32バイト hex) | ランダム生成 (再起動で無効化) |
| `CF_API_TOKEN` | Cloudflare API トークン | 未設定時は Cloudflare デプロイ無効 |
| `CF_ACCOUNT_ID` | Cloudflare アカウント ID | 未設定時は Cloudflare デプロイ無効 |

---

## テスト方針

- [ ] 機能2: `cargo test` で暗号化/復号ラウンドトリップ確認
- [ ] 機能2: curl で `/api/secrets` CRUD を叩いて DB 確認
- [ ] 機能1: ローカルで `dist/` ディレクトリを作成し detect API が返すか確認
- [ ] 機能1: Cloudflare API トークン設定してデプロイ E2E テスト
- [ ] 機能3: 2ユーザー作成→招待リンク→join→共有 secret が両方から見えるか

---

## リスク

- `aes-gcm` クレートの追加でビルド時間が増加 (libssl 不要なのでリスク小)
- `SECRET_VAULT_KEY` 未設定時に XOR fallback にすると実質暗号化なし → 本番では必須化
- Cloudflare Pages Direct Upload は beta API のため仕様変更リスクあり
- チーム機能のファイルシステム共有は `/data/` ボリュームのパス設計が必要 (将来拡張)

---

## 完了条件

- [ ] `cargo build --release` が通る
- [ ] 各 API エンドポイントが curl で期待通りのレスポンスを返す
- [ ] フロントエンドで全 UI 操作が完結する
- [ ] secrets が DB に暗号化状態で保存されていることを確認
- [ ] 招待リンクで別ユーザーがチームに参加できる
- [ ] デプロイボタンから URL が発行される (Cloudflare または Fly)
