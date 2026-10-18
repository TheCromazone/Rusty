def collect(fetch, start=None):
    """Collect items from cursor pages returned by fetch(cursor)."""
    items = []
    cursor = start
    while True:
        page = fetch(cursor)
        if not page["items"]:
            return items
        items.extend(page["items"])
        cursor = page["next"]
        if cursor is None:
            return items
