"""Label generator A."""
import sys


def normalize(text):
    return "-".join(text.split())


def main():
    print(normalize(sys.argv[1]))


if __name__ == "__main__":
    main()
