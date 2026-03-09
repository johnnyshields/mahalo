document.addEventListener('DOMContentLoaded', function() {
    // === Order Form Logic ===
    var orderForm = document.getElementById('order-form');
    var itemsContainer = document.getElementById('items-container');
    var addItemBtn = document.getElementById('add-item-btn');

    var MAX_SCOOPS = 3;

    function addScoopToRow(row) {
        var scoopsList = row.querySelector('.scoops-list');
        var existing = scoopsList.querySelectorAll('.scoop-select');
        if (existing.length >= MAX_SCOOPS) return;

        var template = scoopsList.querySelector('.scoop-select');
        var newScoop = template.cloneNode(true);
        newScoop.selectedIndex = 0;

        var wrapper = document.createElement('div');
        wrapper.className = 'scoop-entry';
        wrapper.appendChild(newScoop);

        var removeBtn = document.createElement('button');
        removeBtn.type = 'button';
        removeBtn.className = 'remove-scoop-btn';
        removeBtn.textContent = '\u00d7';
        removeBtn.onclick = function() { wrapper.remove(); updateAddScoopBtn(row); };
        wrapper.appendChild(removeBtn);

        scoopsList.appendChild(wrapper);
        updateAddScoopBtn(row);
    }

    function updateAddScoopBtn(row) {
        var btn = row.querySelector('.add-scoop-btn');
        var count = row.querySelectorAll('.scoop-select').length;
        if (btn) btn.style.display = count >= MAX_SCOOPS ? 'none' : '';
    }

    // Wire up add-scoop buttons on existing rows
    function wireAddScoopBtns(row) {
        var btn = row.querySelector('.add-scoop-btn');
        if (btn) {
            btn.addEventListener('click', function() { addScoopToRow(row); });
        }
    }

    // Wire initial row
    var initialRow = itemsContainer ? itemsContainer.querySelector('.item-row') : null;
    if (initialRow) wireAddScoopBtns(initialRow);

    if (addItemBtn && itemsContainer) {
        addItemBtn.addEventListener('click', function() {
            var firstRow = itemsContainer.querySelector('.item-row');
            if (!firstRow) return;
            var newRow = firstRow.cloneNode(true);

            // Reset vessel
            var vesselSelect = newRow.querySelector('select[name="vessel"]');
            if (vesselSelect) vesselSelect.selectedIndex = 0;

            // Reset scoops: keep only the first scoop select, remove extras
            var scoopsList = newRow.querySelector('.scoops-list');
            var scoopEntries = scoopsList.querySelectorAll('.scoop-entry');
            scoopEntries.forEach(function(entry) { entry.remove(); });
            var firstScoop = scoopsList.querySelector('.scoop-select');
            if (firstScoop) firstScoop.selectedIndex = 0;

            // Reset topping
            var toppingSelect = newRow.querySelector('select[name="topping"]');
            if (toppingSelect) toppingSelect.selectedIndex = 0;

            // Show add scoop button
            updateAddScoopBtn(newRow);

            // Add remove-item button
            var removeBtn = document.createElement('button');
            removeBtn.type = 'button';
            removeBtn.className = 'remove-item-btn';
            removeBtn.textContent = '\u00d7';
            removeBtn.onclick = function() { newRow.remove(); };
            newRow.appendChild(removeBtn);

            wireAddScoopBtns(newRow);
            itemsContainer.appendChild(newRow);
        });
    }

    // Pre-select flavor from URL ?flavor_id=
    if (itemsContainer) {
        var params = new URLSearchParams(window.location.search);
        var preselect = params.get('flavor_id');
        if (preselect) {
            var firstScoop = itemsContainer.querySelector('.scoop-select');
            if (firstScoop) {
                for (var i = 0; i < firstScoop.options.length; i++) {
                    if (firstScoop.options[i].value === preselect) {
                        firstScoop.selectedIndex = i;
                        break;
                    }
                }
            }
        }
    }

    if (orderForm) {
        orderForm.addEventListener('submit', function(e) {
            e.preventDefault();
            var customerName = orderForm.querySelector('input[name="customer_name"]').value;
            var rows = itemsContainer.querySelectorAll('.item-row');
            var items = [];
            rows.forEach(function(row) {
                var vesselEl = row.querySelector('select[name="vessel"]');
                var vessel = vesselEl ? vesselEl.value : 'cup';

                var scoopSelects = row.querySelectorAll('.scoop-select');
                var scoopFlavors = [];
                scoopSelects.forEach(function(sel) {
                    var val = parseInt(sel.value);
                    if (val) scoopFlavors.push(val);
                });

                var toppingEl = row.querySelector('select[name="topping"]');
                var topping = toppingEl && toppingEl.value ? toppingEl.value : null;
                items.push({ scoop_flavors: scoopFlavors, vessel: vessel, topping: topping });
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

    // === Real-time Price Updates via SSE (menu & flavor detail pages) ===
    var priceElements = document.querySelectorAll('.price[data-flavor-id]');
    if (priceElements.length > 0) {
        var evtSource = new EventSource('/api/prices/stream');
        evtSource.addEventListener('price_updated', function(event) {
            try {
                var payload = JSON.parse(event.data);
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
            } catch (e) {
                console.error('Price SSE parse error:', e);
            }
        });
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
