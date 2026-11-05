import argparse


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="krypper",
        description="Local command-line entry point for Krypper.",
    )
    parser.parse_args(argv)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
