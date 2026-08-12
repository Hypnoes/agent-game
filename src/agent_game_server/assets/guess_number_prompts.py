"""
猜数字提示词 — 改这个文件然后 POST /admin/reload-prompts 即可生效，不用重启。
"""

PROMPTS = {
    "won": (
        "🎉 你猜对了！答案就是 {last_guess}。\n"
        "共用了 {count} 次猜测。\n\n"
        "要再来一局吗？调用 guess_number_start 开始新游戏。"
    ),
    "lost": (
        "😞 游戏结束，用完 {max_attempts} 次机会。\n"
        "答案: {target}\n"
        "历史: {history}\n\n"
        "要再来一局吗？调用 guess_number_start 开始新游戏。"
    ),
    "in_progress": (
        "第 {count} 次: {last_number} → {hint}\n"
        "剩余 {remaining} 次 | 范围 {min_num}-{max_num}\n"
        "历史: {history}\n\n"
        "🎮 **游戏进行中**。调用 guess_number_guess(number=N) 继续。"
    ),
    "in_progress_first": (
        "剩余 {remaining} 次 | 范围 {min_num}-{max_num}\n\n"
        "🎮 **游戏进行中**。调用 guess_number_guess(number=N) 开始猜测。"
    ),
}
