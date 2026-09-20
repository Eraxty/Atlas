from prompt_toolkit import PromptSession

_session = None


def prompt(text=""):
    global _session

    try:
        if _session is None:
            _session = PromptSession()
        return _session.prompt(text)
    except (EOFError, KeyboardInterrupt):
        return "0"
