"""GuessNumberEngine 测试（单 session 实例 + 自描述协议）"""

import time

from agent_game_server import GuessNumberEngine


def test_describe_tools():
    tools = GuessNumberEngine.describe_tools()
    assert len(tools) == 3
    names = [t["name"] for t in tools]
    assert "guess_number_start" in names
    assert "guess_number_guess" in names
    assert "guess_number_state" in names


def test_initial_state():
    e = GuessNumberEngine()
    state = e.get_player_state()
    assert state["public"]["status"] == "in_progress"
    assert state["public"]["guess_count"] == 0
    assert "prompt" in state
    assert "游戏进行中" in state["prompt"]


def test_guess_flow():
    e = GuessNumberEngine()
    # Force target for deterministic test
    e.target = 50

    result = e.handle_action("guess", number=30)
    assert e.guess_count == 1
    assert e.last_result == "too_low"
    assert "太低" in result["prompt"]

    result = e.handle_action("guess", number=60)
    assert e.guess_count == 2
    assert e.last_result == "too_high"

    result = e.handle_action("guess", number=50)
    assert e.status == "won"
    assert e.ended_at is not None
    assert "猜对了" in result["prompt"]


def test_max_attempts():
    e = GuessNumberEngine(max_attempts=3)
    e.target = 50
    for _ in range(3):
        e.handle_action("guess", number=1)  # always too_low
    assert e.status == "lost"
    assert e.ended_at is not None


def test_public_state():
    e = GuessNumberEngine()
    pub = e.get_public_state()
    assert pub["status"] == "in_progress"
    assert "target" in pub  # Admin can see target


def test_player_state_hides_target():
    e = GuessNumberEngine()
    state = e.get_player_state()
    assert "target" not in state["public"]  # Agent can't see target


def test_check_timeout():
    e = GuessNumberEngine()
    # Not timed out yet
    assert e.check_timeout(time.time()) is None
    # Force timeout
    e.last_action = time.time() - 601
    result = e.check_timeout(time.time())
    assert result is not None
    assert result["expired"] is True
    assert e.status == "lost"
