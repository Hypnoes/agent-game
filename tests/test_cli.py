"""CLI 启动路径：token 必须被打印出来，端口必须与文档一致。

这几条不是形式主义：admin token 在未配置环境变量时是**启动时随机生成**的，
打印是运维拿到它的唯一途径。不打印，/admin 就永远打不开。

`uvicorn.run` 在这里被替换成记录器，只验证"传了什么参数进去"，不真的起服务。
"""

import sys

import pytest

from agent_game_server import cli


@pytest.fixture
def captured(monkeypatch, capsys):
    """捕获 uvicorn.run 的调用参数与 stdout。"""

    def scenario(argv):
        calls = []

        def fake_run(app, **kwargs):
            calls.append((app, kwargs))

        monkeypatch.setattr(cli.uvicorn, "run", fake_run)
        monkeypatch.setattr(sys, "argv", argv)
        cli.main()
        return calls, capsys.readouterr().out

    return scenario


def test_help_does_not_start_a_server(captured):
    calls, out = captured(["agent-game-server", "--help"])

    assert calls == [], "--help 不该真的起服务"
    assert "用法" in out
    assert "uv tool install" in out


def test_short_help_flag_works(captured):
    calls, out = captured(["agent-game-server", "-h"])
    assert calls == []
    assert "agent-game-server" in out


def test_startup_prints_the_admin_token(captured):
    """启动时必须把 token 打印出来——它是运维打开 /admin 的唯一途径。"""
    calls, out = captured(["agent-game-server"])

    assert len(calls) == 1
    assert cli._ADMIN_TOKEN in out, "启动输出里没有 admin token"
    assert "token=" in out


def test_startup_prints_where_the_token_came_from(captured, monkeypatch):
    """要说明 token 是配置来的还是随机生成的。"""
    monkeypatch.delenv("AGENT_GAME_ADMIN_TOKEN", raising=False)
    _calls, out = captured(["agent-game-server"])
    assert "随机生成" in out


def test_startup_binds_the_documented_port(captured):
    """端口必须与 README 写的一致，否则文档里的所有 URL 都失效。"""
    calls, out = captured(["agent-game-server"])
    _app, kwargs = calls[0]

    assert kwargs["port"] == 8800
    assert "8800" in out
    # 绑 0.0.0.0 是为了容器/局域网可访问；README 给的是 localhost，两者不矛盾
    assert kwargs["host"] == "0.0.0.0"


def test_startup_uses_the_module_level_app(captured):
    """CLI 起的是 `game_server.app`，与测试里 `create_app()` 的实例是两回事。"""
    from agent_game_server import game_server

    calls, _out = captured(["agent-game-server"])

    assert calls[0][0] is game_server.app


def test_help_takes_precedence_over_starting(captured):
    """`--help` 出现在任意位置都只打印帮助。"""
    calls, out = captured(["agent-game-server", "--help", "--ignored"])

    assert calls == []
    assert "用法" in out
