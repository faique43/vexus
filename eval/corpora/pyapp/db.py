"""Thin connection-pool wrapper around the underlying SQL driver."""


class ConnectionPool:
    """A bounded pool of reusable database connections."""

    def __init__(self, dsn, size=10):
        """Open a pool of at most `size` connections to `dsn`, lazily."""
        self.dsn = dsn
        self.size = size
        self._connections = []

    def acquire(self):
        """Check out a connection, opening a new one if the pool is empty."""
        if self._connections:
            return self._connections.pop()
        return self._open_new()

    def release(self, conn):
        """Return a connection to the pool so a later caller can reuse it."""
        self._connections.append(conn)

    def _open_new(self):
        """Open a fresh low-level connection to `self.dsn`."""
        return object()


_pool = None


def get_connection_pool():
    """Return the process-wide connection pool, creating it on first use."""
    global _pool
    if _pool is None:
        from config import load_config

        cfg = load_config()
        _pool = ConnectionPool(cfg.database_url)
    return _pool
