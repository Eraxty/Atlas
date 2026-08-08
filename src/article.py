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
    filename: str | None = None
    release_name: str | None = None
    part: int | None = None
    total_parts: int | None = None
    file_index: int | None = None
    file_total: int | None = None
    
