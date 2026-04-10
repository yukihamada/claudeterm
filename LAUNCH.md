# ChatWeb Launch Posts

## X (Twitter) — 日本語

### Thread (1/5)
```
ChatWeb をリニューアルしました 🚀

ブラウザだけで Claude Code が動く開発ターミナル。
インストール不要、ログインするだけ。

「Todoアプリ作って」→ 5分で完成 → デプロイ → 公開URL取得

無料で始められます → chatweb.ai
```

### (2/5)
```
何ができるの？

💻 コード生成 — React, Rust, Python なんでも
🚀 ワンクリックデプロイ — Fly.io に即公開
🐛 バグ修正 — エラー貼るだけで原因特定→修正
📱 iOSアプリ — Swift → TestFlight → App Store
🎨 画像生成 — /image コマンドで
📊 データ分析 — pandas + matplotlib
```

### (3/5)
```
他のツールとの違い：

vs Cursor → インストール不要、ブラウザ完結
vs Copilot → 補完だけじゃなくファイル編集+実行+デプロイまで
vs Replit → Claude Code 搭載で圧倒的に賢い
vs Claude.ai → コード実行・ファイル管理・デプロイが一気通貫
```

### (4/5)
```
新機能：

🔄 ワークフローパイプライン（Web/モバイル/動画/リサーチ/デザイン）
👁 ライブプレビュー（作ったアプリがリアルタイムで見える）
🤝 セッション共有（URLで友達にシェア、共同編集）
⏲ 定期実行（cron、テスト自動実行）
📁 テンプレートスターター（6種類）
🔐 セキュアキー管理
```

### (5/5)
```
無料で $3 分付き。友達招待でお互い $3 もらえる 🎁

ステップバイステップのガイド付きだから
プログラミング経験ゼロでも大丈夫。

今すぐ試す → chatweb.ai
デモ動画 → chatweb.ai/demo

#ChatWeb #ClaudeCode #AI開発 #プログラミング
```

## X (Twitter) — English

### Thread (1/3)
```
Just launched ChatWeb 🚀

Claude Code running in your browser. No install, no setup.

Type "build me a todo app" → watch AI write files, run commands, deploy → get a live URL in 5 minutes.

Free to start → chatweb.ai
```

### (2/3)
```
What sets it apart:

✅ Browser-only — no terminal setup needed
✅ Full execution — not just code completion
✅ Live preview — see your app as it's built
✅ Deploy built-in — Fly.io, one command
✅ Session sharing — fork or collaborate via URL
✅ Workflow guides — Web, Mobile, Video, Research, Design
```

### (3/3)
```
Built with Rust + axum + SQLite + Fly.io

Open source: github.com/yukihamada/claudeterm

Free $3 credit on signup. Invite friends, both get $3.

Try it → chatweb.ai
Watch demo → chatweb.ai/demo

#ChatWeb #ClaudeCode #AI #DevTools
```

## Hacker News

### Title
Show HN: ChatWeb – Claude Code in your browser, no install needed

### Text
Hi HN, I built ChatWeb (https://chatweb.ai) — a browser-based terminal that runs Claude Code.

The idea: you open a browser, type "build me a web app", and Claude writes files, runs commands, executes tests, and deploys — all streamed to your browser in real-time.

Tech stack: Rust (axum), SQLite, Claude Code CLI via PTY, Fly.io

Features:
- Live preview (iframe shows your app as Claude builds it)
- Workflow pipelines (step-by-step guides for Web/Mobile/Video/Research)
- Session sharing (share URL, fork or collaborate)
- Template starters (React, Next.js, Rust API, Python ML, etc.)
- Cron jobs (scheduled background tasks)
- Secure key management (keys encrypted at rest, auto-injected into Claude sessions)

It's free to start ($3 credit included). The code is open source: https://github.com/yukihamada/claudeterm

I'd love to hear what you think. What would make this more useful?

## Product Hunt

### Tagline
Claude Code in your browser — build & deploy in minutes

### Description
**ChatWeb** turns your browser into a full Claude Code terminal.

**The problem**: Running Claude Code locally requires Node.js, CLI setup, API keys, and a dev environment. Not everyone has that — or wants to maintain it.

**The solution**: Open chatweb.ai, sign in, tell Claude what to build. It writes code, installs packages, runs tests, and deploys — all from one browser tab.

**What's included:**
- Full terminal with Claude Code (Sonnet 4)
- Pre-installed: Node.js, Python, Rust, Go, Ruby
- One-click deploy to Fly.io, Vercel, Netlify, Cloudflare, Railway
- Subdomain hosting: your-app.chatweb.ai
- Veo 3 video generation built-in
- Works on any device: iPad, Chromebook, phone
- $3 free credit, no credit card needed

**Built with:** Rust (axum) + Fly.io + SQLite

### First Comment
Hey PH! I built this because I wanted to use Claude Code from my iPad and share dev environments without "works on my machine" issues.

The whole thing is a single Rust binary — ~5K lines of axum serving WebSocket connections to Claude CLI processes. Each user gets an isolated workspace with Node/Python/Rust pre-installed.

Try it: https://chatweb.ai

---

## Demo Video Script (60 sec)

[0-5s] "What if you could use Claude Code from any browser?"
[5-15s] Open chatweb.ai → sign in with Google (2-click flow)
[15-25s] Type: "Build a React landing page with hero, features, and pricing"
[25-40s] Claude writing files, installing deps, running build (speed up 4x)
[40-50s] "Deploy to Fly.io" → deploy completing, URL appearing
[50-55s] Open the deployed URL → show the finished site
[55-60s] "chatweb.ai — Claude Code in your browser. Free to start."

---

## Key Stats for Press
- Single Rust binary (~5K lines)
- 5 language runtimes pre-installed (Node, Python, Rust, Go, Ruby)
- 5 deploy platforms (Fly.io, Vercel, Cloudflare, Netlify, Railway)
- Response latency: ~350ms
- Pricing: Free $3 → Starter $9/mo → Pro $29/mo → Power $79/mo
- Stack: Rust + axum + SQLite + Fly.io
- Open source: github.com/yukihamada/claudeterm
