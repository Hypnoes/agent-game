import os as _os
import uuid as _uuid

_os.environ.setdefault("AGENT_GAME_ADMIN_TOKEN", str(_uuid.uuid4()))

from agent_game_server.cli import main  # noqa: E402 — 必须在设置环境变量之后导入
from agent_game_server.game_server import app  # noqa: E402,F401 — uvicorn main:app 的入口

if __name__ == "__main__":
    main()
