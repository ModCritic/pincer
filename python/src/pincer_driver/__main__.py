"""CLI entry point for pincer Python wrapper.

This module is invoked when running:
    python -m pincer_driver [args...]
or via the installed script:
    pincer [args...]
"""

import sys
from .wrapper import run_pincer


def main() -> None:
    """Main entry point for the pincer CLI."""
    exit_code = run_pincer()
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
