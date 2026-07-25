import os
from utils.text import slugify


TIMEOUT = 30


# Doubles then adds.
def top_level(a, b):
    """Adds."""
    return helper(a) + b


def helper(x):
    return slugify(x)


class Greeter:
    def __init__(self, name):
        self.name = name

    def greet(self, punct):
        return f"hi {self.name}{punct}"
