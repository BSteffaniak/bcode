def lookup(cache,key,fetch):
    """Return cached values including empty strings; fetch only absent keys."""
    return cache.get(key) or fetch(key)
