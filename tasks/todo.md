# ChatWeb 次期計画

## Phase 1: マルチプラットフォームデプロイ (今すぐ)

### 1.1 Dockerfile — CLI追加
- [x] `flyctl` (Fly.io)
- [x] `vercel` (Vercel)
- [ ] `wrangler` (Cloudflare Workers/Pages)
- [ ] `netlify-cli` (Netlify)
- [ ] `@railway/cli` (Railway)
- [ ] `supabase` (Supabase Functions)

### 1.2 プロンプト — デプロイ案内拡張
- [x] CLAUDE.md base_rules に Fly.io/Vercel 追加済み
- [x] ユーザーKeys検出 → 利用可能デプロイ先の自動案内
- [ ] 新プラットフォーム分のプロンプト追加
- 対応トークン:
  - `FLY_API_TOKEN` → `fly deploy`
  - `VERCEL_TOKEN` → `vercel --yes`
  - `CLOUDFLARE_API_TOKEN` → `wrangler deploy`
  - `NETLIFY_AUTH_TOKEN` → `netlify deploy --prod`
  - `RAILWAY_TOKEN` → `railway up`
  - `SUPABASE_ACCESS_TOKEN` → `supabase functions deploy`

### 1.3 UI — デプロイモーダル強化
- [ ] プラットフォーム別タブ（ChatWeb / Fly / Vercel / CF / Netlify / Railway）
- [ ] 各トークン設定状況の表示
- [ ] ワンクリック「Vercelにデプロイ」ボタン

## Phase 2: KAGI連携 (1-2日)

### 2.1 Vault同期 — KAGIアプリ ↔ ChatWeb
- [ ] KAGI iOS の ChatWebVaultSync → `/api/v1/vault` 同期確認
- [ ] 6桁コード方式のペアリング確認（既存の転送コード機能）
- [ ] 同期キーを Claude CLI 環境変数に自動注入

### 2.2 スマートホーム操作 — ChatWebから
- [ ] CLAUDE.md に KAGI API情報を追加:
  - `POST /api/v1/devices/:id/unlock` — 解錠
  - `POST /api/v1/devices/:id/lock` — 施錠
  - `GET /api/v1/properties` — 物件一覧
  - `GET /api/v1/reservations` — 予約一覧
- [ ] Claude が curl で KAGI API を叩けるよう KAGI_SERVER_URL 設定
- [ ] セキュリティ: 操作前の確認プロンプト

### 2.3 予約連携 — Beds24 → ChatWeb
- [ ] 「今日のチェックイン一覧」をチャットで取得
- [ ] 「清掃スケジュール」自動生成
- [ ] Cronジョブ: 毎朝予約サマリー

### 2.4 KAGIテンプレート
- [ ] 「スマートホームダッシュボード」テンプレート追加
- [ ] 物件一覧+デバイス状態+予約カレンダーのWebアプリ

## Phase 3: 開発者体験 (3-5日)

### 3.1 GitHub連携強化
- [ ] `gh repo create` → push → Vercel/Netlify自動連携
- [ ] PRテンプレート自動生成
- [ ] GitHub Actions ワークフロー作成支援

### 3.2 データベース統合
- [ ] Supabase: テーブル設計 → マイグレーション
- [ ] Turso/PlanetScale: SQLite接続
- [ ] `.env` にDB接続文字列を自動設定

### 3.3 カスタムドメイン
- [ ] `my-app.chatweb.ai` → `myapp.com` のCNAME設定UI
- [ ] Cloudflare DNS API経由で自動設定

### 3.4 テンプレート拡充
- [ ] Next.js + Vercel
- [ ] Rust + Fly.io
- [ ] React + Cloudflare Pages
- [ ] Express + Railway

## 優先順位
1. **Phase 1.1** — CLI追加 (Dockerfile 1行ずつ)
2. **Phase 1.2** — プロンプト拡張 (コスト0)
3. **Phase 2.1** — KAGI Vault同期 (既存コード接続)
4. **Phase 2.2** — スマートホーム操作 (プロンプトのみ)
5. **Phase 1.3** — UIデプロイ強化
6. **Phase 2.3-2.4** — 予約連携+テンプレート
7. **Phase 3** — 開発者体験
