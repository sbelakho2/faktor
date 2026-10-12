"""Label generator B."""
import sys


def normalize(text):
    return text.strip().lower().replace(" ", "_")


def main():
    print(normalize(sys.argv[1]))


if __name__ == "__main__":
    main()
