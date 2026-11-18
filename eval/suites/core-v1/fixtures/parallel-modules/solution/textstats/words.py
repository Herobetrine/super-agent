import re


def word_count(text):
    counts = {}
    for word in re.findall(r"[A-Za-z0-9']+", text.lower()):
        counts[word] = counts.get(word, 0) + 1
    return counts
