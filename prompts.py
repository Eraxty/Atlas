from prompt_toolkit import PromptSession

session = PromptSession()


def prompt(text=""):
    return session.prompt(text)
