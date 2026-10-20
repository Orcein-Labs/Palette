"""Reject writable mounts that expose protected host paths."""
from pathlib import Path


def require_disjoint(writable, protected, parents=()):
    """Refuse a writable path that equals, contains or lies inside a protected
    one, or that equals one of ``parents`` (``provisioning.command_parents``)."""
    parents = {Path(parent).resolve() for parent in parents}
    for writable_path in writable:
        left = Path(writable_path).resolve()
        if left in parents:
            raise PermissionError(f"Writable path {left} must not be the parent of a host-command path")
        for protected_path in protected:
            right = Path(protected_path).resolve()
            if left == right or left.is_relative_to(right) or right.is_relative_to(left):
                raise PermissionError(f"Writable path {left} must not overlap protected path {right}")


def require_preserved_roots(writable, roots):
    for writable_path in writable:
        left = Path(writable_path).resolve()
        for root in roots:
            if Path(root).is_relative_to(left):
                raise PermissionError(f"Writable path {left} must not overlap sandbox root {root}")


def parse_read_roots(spec):
    """Host directories the kernel may read (PALETTE_READ_ROOTS, ':'-separated
    absolute paths), each bound read-only at its own path."""
    roots = []
    for entry in (spec or "").split(":"):
        if not entry:
            continue
        if not Path(entry).is_absolute():
            raise PermissionError(f"PALETTE_READ_ROOTS entries must be absolute paths: {entry}")
        root = Path(entry).resolve()
        if not root.is_dir():
            raise PermissionError(f"read root is not a directory: {root}")
        roots.append(str(root))
    return roots


def trusted_tools_dir(spec):
    """The directory of human-approved kernel tools (PALETTE_TRUSTED_TOOLS),
    created if missing. The kernel reads it; only the patch broker writes it."""
    if not spec:
        return None
    if not Path(spec).is_absolute():
        raise ValueError("PALETTE_TRUSTED_TOOLS must be an absolute path")
    Path(spec).mkdir(parents=True, exist_ok=True, mode=0o700)
    return str(Path(spec).resolve())


def require_read_roots(roots, writable, others):
    """A read root may not contain any other mount, so no mount order can let
    it hide one, and may not lie inside a writable mount."""
    for root in roots:
        r = Path(root).resolve()
        for path in list(writable) + list(others):
            p = Path(path).resolve() if Path(path).exists() else Path(path)
            if p == r or p.is_relative_to(r):
                raise PermissionError(f"read root {r} contains the mount {p}")
        for path in writable:
            if r.is_relative_to(Path(path).resolve()):
                raise PermissionError(f"read root {r} lies inside the writable mount {Path(path).resolve()}")
