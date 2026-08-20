from prompt_toolkit import PromptSession

session = PromptSession()


def prompt(text=""):
    try:
        return session.prompt(text)
    except (EOFError, KeyboardInterrupt):
        return "0"
