"""Label generator B."""
import sys

from common import normalize


def main():
    print(normalize(sys.argv[1]))


if __name__ == "__main__":
    main()
