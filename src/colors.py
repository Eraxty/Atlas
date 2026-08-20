import sys
from colorama import Fore, Style

if sys.stdout.isatty():
    reset = Style.RESET_ALL
    bold = Style.BRIGHT
    dim = Style.DIM
    red = Fore.RED
    green = Fore.GREEN
    yellow = Fore.YELLOW
    magenta = Fore.MAGENTA
    cyan = Fore.CYAN
else:
    reset = bold = dim = red = green = yellow = magenta = cyan = ""
