"""
agent-game-server CLI

启动 Game Server。

用法：
    agent-game-server
    uv run main.py
"""

import os as _os
import sys as _sys
import signal as _signal
import uuid as _uuid
import uvicorn

from agent_game_server.game_server import app  # verify 用 main:app


def main():
    if len(_sys.argv) > 1 and _sys.argv[1] in ("-h", "--help"):
        print("agent-game-server — AI Agent 游戏服务端")
        print()
        print("用法:")
        print("  agent-game-server")
        print()
        print("安装:")
        print("  uv tool install .")
        return

    try:
        import fastapi  # noqa: F401
    except ImportError:
        print("❌ fastapi 未安装。请重新安装：")
        print("  uv tool install --reinstall .")
        raise SystemExit(1)

    _signal.signal(_signal.SIGINT, lambda s, f: _sys.exit(0))

    # Admin 内部查询密钥
    _os.environ.setdefault("AGENT_GAME_ADMIN_TOKEN", str(_uuid.uuid4()))

    print("🎮 agent-game starting...")
    print("   Game API:   http://localhost:8800")
    print("   Admin:      http://localhost:8800/admin")
    print()

    uvicorn.run(app, host="0.0.0.0", port=8800, log_level="warning")


if __name__ == "__main__":
    main()
