def line_stats(text):
    lines = text.splitlines()
    return {
        "lines": len(lines),
        "blank": sum(1 for line in lines if not line.strip()),
        "longest": max((len(line) for line in lines), default=0),
    }
