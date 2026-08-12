import os as _os
import uuid as _uuid

_os.environ.setdefault("AGENT_GAME_ADMIN_TOKEN", str(_uuid.uuid4()))

from agent_game_server.cli import main

if __name__ == "__main__":
    main()
