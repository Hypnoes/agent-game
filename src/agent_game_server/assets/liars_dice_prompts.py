"""
大话骰提示词 — 改这个文件然后 POST /admin/reload-prompts 即可生效，不用重启。
"""

PROMPTS = {
    "finished": (
        "🏆 游戏结束！{winner} 获胜！\n"
        "全场骰子: {all_dice}\n"
        "叫数历史: {call_history}\n\n"
        "要再来一局吗？调用 liars_dice_join 重新匹配。"
    ),
    "waiting": (
        "🎲 已加入匹配队列！当前 {players}/{needed} 人\n"
        "⏳ 等待对手加入…已等待 {waited} 秒（匹配超时: 120 秒）\n"
        "**不要重启 MCP 或重新加入** — 服务器自动匹配，放弃只会让你失去位置。\n"
        "10 秒后调用 liars_dice_state 查看。"
    ),
    "not_your_turn": (
        "⏳ 等待 {current_player} 行动中…({progress}){patience}\n"
        "当前叫数: {call_str}\n"
        "你的骰子: {dice}\n\n"
        "🔄 这不是你的回合。**不要重启或放弃游戏。**\n"
        "10 秒后调用 liars_dice_state(player_index={player_index}) 查看。"
    ),
    "your_turn": (
        "🎲 **轮到你了！这是你的回合，必须立即行动。**{msg_line}\n"
        "你的骰子: {dice}\n"
        "当前叫数: {call_str}\n"
        "⏱ 剩余 {remaining:.0f} 秒（超时自动判负）\n\n"
        "**行动（选一个调用，必须带 player_token）：**\n"
        "  liars_dice_bid(player_index={player_index}, player_token='<你的token>', quantity=N, face=F, message=\"?\")  — 加码\n"
        "  liars_dice_challenge(player_index={player_index}, player_token='<你的token>')  — 开盅\n\n"
        "💡 叫数必须比上家大。1 是百搭。叫 1 时 1 只算 1。\n"
        "⚠️ 再调 liars_dice_state 不会改变局面，只会浪费你的回合时间。"
    ),
    "patience_20": ("😤 对手还在思考——这是正常的。60 秒超时，别急。"),
    "patience_40": (
        "⏰ 只剩 {remaining_time} 秒了。如果超时对手自动判负，耐心等就好。"
    ),
}
