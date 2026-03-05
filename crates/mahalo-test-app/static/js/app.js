document.addEventListener('DOMContentLoaded', function() {
    // === Order Form Logic ===
    var orderForm = document.getElementById('order-form');
    var itemsContainer = document.getElementById('items-container');
    var addItemBtn = document.getElementById('add-item-btn');

    if (addItemBtn && itemsContainer) {
        addItemBtn.addEventListener('click', function() {
            var firstRow = itemsContainer.querySelector('.item-row');
            if (!firstRow) return;
            var newRow = firstRow.cloneNode(true);
            newRow.querySelector('select[name="flavor_id"]').selectedIndex = 0;
            newRow.querySelector('input[name="scoops"]').value = 1;
            var toppingSelect = newRow.querySelector('select[name="topping"]');
            if (toppingSelect) toppingSelect.selectedIndex = 0;

            var removeBtn = document.createElement('button');
            removeBtn.type = 'button';
            removeBtn.className = 'remove-item-btn';
            removeBtn.textContent = '\u00d7';
            removeBtn.onclick = function() { newRow.remove(); };
            newRow.appendChild(removeBtn);

            itemsContainer.appendChild(newRow);
        });
    }

    if (orderForm) {
        orderForm.addEventListener('submit', function(e) {
            e.preventDefault();
            var customerName = orderForm.querySelector('input[name="customer_name"]').value;
            var rows = itemsContainer.querySelectorAll('.item-row');
            var items = [];
            rows.forEach(function(row) {
                var flavorId = parseInt(row.querySelector('select[name="flavor_id"]').value);
                var scoops = parseInt(row.querySelector('input[name="scoops"]').value) || 1;
                var toppingEl = row.querySelector('select[name="topping"]');
                var topping = toppingEl && toppingEl.value ? toppingEl.value : null;
                items.push({ flavor_id: flavorId, scoops: scoops, topping: topping });
            });

            fetch('/api/orders', {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json',
                    'x-api-key': 'demo-key'
                },
                body: JSON.stringify({ customer_name: customerName, items: items })
            })
            .then(function(res) { return res.json(); })
            .then(function(data) {
                if (data.id) {
                    window.location.href = '/orders/' + data.id;
                } else {
                    alert('Error placing order: ' + JSON.stringify(data));
                }
            })
            .catch(function(err) { alert('Error: ' + err.message); });
        });
    }

    // === Order Status WebSocket ===
    var orderData = document.getElementById('order-data');
    if (orderData) {
        var orderId = orderData.dataset.orderId;
        var statusEl = document.getElementById('order-status');
        var wsStatusEl = document.getElementById('ws-status');
        var msgRef = 1;
        var heartbeatInterval = null;

        var statusEmojis = {
            pending: '\u23f3 Pending',
            preparing: '\ud83d\udc68\u200d\ud83c\udf73 Preparing',
            ready: '\u2705 Ready!',
            delivered: '\ud83c\udf89 Delivered!'
        };

        function updateStatus(status) {
            if (!statusEl) return;
            statusEl.textContent = statusEmojis[status] || status;
            statusEl.className = 'badge badge-large ' + status;

            var steps = document.querySelectorAll('.timeline-step');
            var statuses = ['pending', 'preparing', 'ready', 'delivered'];
            var idx = statuses.indexOf(status);
            steps.forEach(function(step, i) {
                if (i <= idx) {
                    step.classList.add('active');
                } else {
                    step.classList.remove('active');
                }
            });
        }

        function connect() {
            var protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            var wsUrl = protocol + '//' + window.location.host + '/ws';
            var ws = new WebSocket(wsUrl);

            ws.onopen = function() {
                if (wsStatusEl) {
                    wsStatusEl.textContent = '\u2705 Connected to live updates';
                    wsStatusEl.className = 'ws-connected';
                }
                var joinMsg = {topic: 'order:' + orderId, event: 'phx_join', payload: { customer_name: 'Web User' }, ref: String(msgRef)};
                ws.send(JSON.stringify(joinMsg));
                msgRef++;

                heartbeatInterval = setInterval(function() {
                    if (ws.readyState === WebSocket.OPEN) {
                        ws.send(JSON.stringify({topic: 'phoenix', event: 'heartbeat', payload: {}, ref: String(msgRef)}));
                        msgRef++;
                    }
                }, 30000);
            };

            ws.onmessage = function(event) {
                try {
                    var msg = JSON.parse(event.data);
                    var eventName = msg.event;
                    var payload = msg.payload;

                    if (eventName === 'phx_reply' && payload && payload.response) {
                        if (payload.response.status) {
                            updateStatus(payload.response.status);
                        }
                    } else if (eventName === 'status_changed' && payload) {
                        updateStatus(payload.status);
                    }
                } catch (e) {
                    console.error('WS parse error:', e);
                }
            };

            ws.onclose = function() {
                if (wsStatusEl) {
                    wsStatusEl.textContent = '\ud83d\udd0c Disconnected - Reconnecting...';
                    wsStatusEl.className = 'ws-disconnected';
                }
                clearInterval(heartbeatInterval);
                setTimeout(connect, 3000);
            };

            ws.onerror = function() {
                if (wsStatusEl) {
                    wsStatusEl.textContent = '\u274c Connection error';
                    wsStatusEl.className = 'ws-error';
                }
            };
        }

        connect();
    }

    // === Real-time Price Updates (menu & flavor detail pages) ===
    var priceElements = document.querySelectorAll('.price[data-flavor-id]');
    if (priceElements.length > 0) {
        var protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        var wsUrl = protocol + '//' + window.location.host + '/ws';
        var priceRef = 1;

        function connectPrices() {
            var ws = new WebSocket(wsUrl);

            ws.onopen = function() {
                var joinMsg = {topic: 'store:lobby', event: 'phx_join', payload: { customer_name: 'Price Watcher' }, ref: String(priceRef)};
                ws.send(JSON.stringify(joinMsg));
                priceRef++;

                setInterval(function() {
                    if (ws.readyState === WebSocket.OPEN) {
                        ws.send(JSON.stringify({topic: 'phoenix', event: 'heartbeat', payload: {}, ref: String(priceRef)}));
                        priceRef++;
                    }
                }, 30000);
            };

            ws.onmessage = function(event) {
                try {
                    var msg = JSON.parse(event.data);
                    var eventName = msg.event;
                    var payload = msg.payload;

                    if (eventName === 'price_updated' && payload) {
                        var flavorId = payload.flavor_id;
                        var newPrice = payload.price;
                        priceElements.forEach(function(el) {
                            if (el.dataset.flavorId === String(flavorId)) {
                                el.textContent = newPrice;
                                el.classList.add('price-updated');
                                setTimeout(function() { el.classList.remove('price-updated'); }, 600);
                            }
                        });

                        var toast = document.getElementById('price-update-toast');
                        if (toast) {
                            toast.textContent = '\ud83d\udcb0 ' + payload.flavor_name + ' is now ' + newPrice + '!';
                            toast.className = 'toast visible';
                            setTimeout(function() { toast.className = 'toast hidden'; }, 3000);
                        }
                    }
                } catch (e) {
                    console.error('Price WS parse error:', e);
                }
            };

            ws.onclose = function() {
                setTimeout(connectPrices, 5000);
            };
        }

        connectPrices();
    }

    // === Global Chat Widget ===
    var chatWidgetBtn = document.querySelector('.chat-widget-btn');
    var chatWidgetPanel = document.querySelector('.chat-widget-panel');
    var chatWidgetMessages = document.getElementById('chat-widget-messages');
    var chatWidgetInput = document.getElementById('chat-widget-input');
    var chatWidgetSend = document.getElementById('chat-widget-send');

    var chatWidgetClose = document.getElementById('chat-widget-close');

    if (chatWidgetBtn && chatWidgetPanel) {
        chatWidgetBtn.addEventListener('click', function() {
            chatWidgetPanel.classList.toggle('open');
        });
        if (chatWidgetClose) {
            chatWidgetClose.addEventListener('click', function() {
                chatWidgetPanel.classList.remove('open');
            });
        }
    }

    if (chatWidgetMessages) {
        var chatRef = 1;
        var chatWs = null;
        var chatHeartbeatInterval = null;

        function addChatMessage(text, isUser) {
            var msgDiv = document.createElement('div');
            msgDiv.className = 'chat-message ' + (isUser ? 'user' : 'bot');

            var avatar = document.createElement('span');
            avatar.className = 'chat-avatar';
            avatar.textContent = isUser ? '\ud83d\ude0a' : '\ud83c\udf66';

            var bubble = document.createElement('div');
            bubble.className = 'chat-bubble';
            bubble.innerHTML = text;

            msgDiv.appendChild(avatar);
            msgDiv.appendChild(bubble);
            chatWidgetMessages.appendChild(msgDiv);
            chatWidgetMessages.scrollTop = chatWidgetMessages.scrollHeight;
        }

        function addTypingIndicator() {
            var msgDiv = document.createElement('div');
            msgDiv.className = 'chat-message bot';
            msgDiv.id = 'chat-widget-typing';

            var avatar = document.createElement('span');
            avatar.className = 'chat-avatar';
            avatar.textContent = '\ud83c\udf66';

            var bubble = document.createElement('div');
            bubble.className = 'chat-bubble typing';
            bubble.textContent = 'typing...';

            msgDiv.appendChild(avatar);
            msgDiv.appendChild(bubble);
            chatWidgetMessages.appendChild(msgDiv);
            chatWidgetMessages.scrollTop = chatWidgetMessages.scrollHeight;
        }

        function removeTypingIndicator() {
            var el = document.getElementById('chat-widget-typing');
            if (el) el.remove();
        }

        function connectChat() {
            var protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            var wsUrl = protocol + '//' + window.location.host + '/ws';
            chatWs = new WebSocket(wsUrl);

            chatWs.onopen = function() {
                chatWs.send(JSON.stringify({topic: 'support:chat', event: 'phx_join', payload: {customer_name: 'Guest'}, ref: String(chatRef)}));
                chatRef++;

                chatHeartbeatInterval = setInterval(function() {
                    if (chatWs && chatWs.readyState === WebSocket.OPEN) {
                        chatWs.send(JSON.stringify({topic: 'phoenix', event: 'heartbeat', payload: {}, ref: String(chatRef)}));
                        chatRef++;
                    }
                }, 30000);
            };

            chatWs.onmessage = function(event) {
                try {
                    var msg = JSON.parse(event.data);
                    var eventName = msg.event;
                    var payload = msg.payload;

                    if (eventName === 'phx_reply' && payload && payload.response) {
                        removeTypingIndicator();
                        if (payload.response.message) {
                            addChatMessage(payload.response.message, false);
                        }
                    } else if (eventName === 'chat_reply' && payload) {
                        removeTypingIndicator();
                        if (payload.message) {
                            addChatMessage(payload.message, false);
                        }
                    }
                } catch (e) {
                    console.error('Chat WS parse error:', e);
                }
            };

            chatWs.onclose = function() {
                clearInterval(chatHeartbeatInterval);
                setTimeout(connectChat, 3000);
            };
        }

        function sendChatMessage() {
            if (!chatWidgetInput) return;
            var text = chatWidgetInput.value.trim();
            if (!text) return;
            addChatMessage(text, true);
            chatWidgetInput.value = '';

            if (chatWs && chatWs.readyState === WebSocket.OPEN) {
                addTypingIndicator();
                chatWs.send(JSON.stringify({topic: 'support:chat', event: 'chat_message', payload: {message: text}, ref: String(chatRef)}));
                chatRef++;
            }
        }

        if (chatWidgetSend) {
            chatWidgetSend.addEventListener('click', sendChatMessage);
        }
        if (chatWidgetInput) {
            chatWidgetInput.addEventListener('keydown', function(e) {
                if (e.key === 'Enter') sendChatMessage();
            });
        }

        connectChat();
    }
});
