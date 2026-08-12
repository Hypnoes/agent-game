"""大话骰引擎测试（单 session 实例 + 自描述协议）"""

import time

import pytest

from agent_game_server.games.liars_dice import LiarsDiceEngine


def test_describe_tools():
    tools = LiarsDiceEngine.describe_tools()
    assert len(tools) == 4
    names = [t["name"] for t in tools]
    assert "liars_dice_join" in names
    assert "liars_dice_bid" in names
    assert "liars_dice_challenge" in names


def test_add_first_player_waits():
    e = LiarsDiceEngine()
    r = e.handle_action("join", player_name="Alice")
    assert r["player_index"] == 0
    assert r["phase"] == "waiting"


def test_add_second_player_starts():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    r = e.handle_action("join", player_name="Bob")
    assert r["player_index"] == 1
    assert r["phase"] == "bidding"
    assert len(r["dice"]) == 5


def test_player_state_hides_opponent_dice():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    state = e.get_player_state(0)
    assert len(state["private"]["your_dice"]) == 5
    assert "all_dice" not in state["private"]


def test_first_bid_always_valid():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    turn = e.current_turn
    e.handle_action("bid", player_index=turn, quantity=2, face=3)
    assert e.current_call is not None
    assert e.current_call.quantity == 2


def test_higher_bid_valid():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    turn = e.current_turn
    e.handle_action("bid", player_index=turn, quantity=2, face=3)
    next_turn = e.current_turn
    e.handle_action("bid", player_index=next_turn, quantity=3, face=3)
    assert e.current_call is not None
    assert e.current_call.quantity == 3


def test_lower_bid_invalid():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    turn = e.current_turn
    e.handle_action("bid", player_index=turn, quantity=3, face=3)
    next_turn = e.current_turn
    with pytest.raises(ValueError, match="必须比上家大"):
        e.handle_action("bid", player_index=next_turn, quantity=2, face=4)


def test_challenge():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    turn = e.current_turn
    e.handle_action("bid", player_index=turn, quantity=1, face=3)
    next_turn = e.current_turn
    state = e.handle_action("challenge", player_index=next_turn)
    assert e.phase == "finished"
    assert "winner_name" in state["private"]


def test_bid_message():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    turn = e.current_turn
    e.handle_action("bid", player_index=turn, quantity=2, face=3, message="bluff")
    assert e.current_call is not None
    assert e.current_call.message == "bluff"


def test_turn_timeout():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    e.last_action = time.time() - 61
    result = e.check_timeout(time.time())
    assert result is not None
    assert result["type"] == "turn_timeout"
    assert e.phase == "finished"


def test_lobby_timeout():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.last_action = time.time() - 121
    result = e.check_timeout(time.time())
    assert result is not None
    assert result["type"] == "lobby_timeout"
    assert result["expired"] is True


def test_prompt_in_state():
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    state = e.get_player_state(0)
    assert "prompt" in state
    assert len(state["prompt"]) > 10
