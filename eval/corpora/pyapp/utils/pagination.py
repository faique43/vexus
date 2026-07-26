"""Pagination helper shared by every `list_*` service function."""

PAGE_SIZE = 20


def paginate(items, page):
    """Slice `items` down to the given 1-indexed `page` using the shared page size."""
    start = (page - 1) * PAGE_SIZE
    return items[start : start + PAGE_SIZE]
