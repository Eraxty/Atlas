from dataclasses import dataclass

@dataclass
class Article:
    number: int
    subject: str
    author: str
    date: str
    message_id: str
    references: str
    bytes: int
    lines: int