import re


def slugify(text):
    slug = re.sub(r"[\s-]+", "-", text.strip().lower())
    return slug.strip("-")
