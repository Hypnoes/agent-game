"""
agent-game-server CLI

启动 Game Server。

用法：
    agent-game-server
    uv run main.py
"""

import os as _os
import signal as _signal
import sys as _sys

import uvicorn

from agent_game_server.game_server import _ADMIN_TOKEN, app  # verify 用 main:app


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

    # token 已由 game_server 在导入时解析（环境变量或随机生成）。
    # 这里只把它打印出来——随机生成的分支没有别的途径能拿到它，
    # 不打印运维就打不开 /admin 的鉴权页面。
    source = (
        "来自 AGENT_GAME_ADMIN_TOKEN"
        if _os.environ.get("AGENT_GAME_ADMIN_TOKEN")
        else "本次运行随机生成"
    )

    print("🎮 agent-game starting...")
    print("   Game API:   http://localhost:8800")
    print(f"   Admin:      http://localhost:8800/admin?token={_ADMIN_TOKEN}")
    print(f"   Admin 查询头: X-Admin-Token: {_ADMIN_TOKEN}")
    print(f"   Admin token {source}")
    print()

    uvicorn.run(app, host="0.0.0.0", port=8800, log_level="warning")


if __name__ == "__main__":
    main()
